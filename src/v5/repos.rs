//! `ReposService` — v5 repos namespace (V5REPOS-2..14).
//!
//! Methods:
//! * `forge_port_schema` — wire-surfaced capability introspection (V5REPOS-2).
//! * `list`, `get`, `add`, `remove`, `add_remote`, `remove_remote`
//!   — CRUD over per-org YAML (V5REPOS-3..8).
//! * `sync`, `push` — metadata drift/push via `ForgePort` (V5REPOS-13, 14).
//!
//! Provider derivation (V5REPOS-12) runs on every call that resolves a
//! remote's provider — on the wire, every `Remote` event carries its
//! derived provider.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_stream::stream;
use futures::Stream;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::v5::adapters::{
    DriftFieldKind, ForgeMetadata, ForgePortError,
    MetadataFields, ProviderVisibility,
};
use crate::v5::config::{
    load_all, load_orgs, save_org, ConfigError, CredentialType, DomainName, GlobalConfig, OrgConfig,
    OrgName, OrgRepo, ProviderKind, Remote, RemoteUrl, RepoMetadataLocal, RepoName, RepoRef,
};
use crate::v5::secrets::{SecretResolver, YamlSecretStore};

// ---------------------------------------------------------------------
// Events.
// ---------------------------------------------------------------------

/// Event surface for the repos namespace. All events are flat
/// `snake_case` to match the harness's jq assertions.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RepoEvent {
    /// `mirror`: what a push to one forge would send (dry run).
    MirrorPlan {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        url: String,
        provider: String,
        refspecs: Vec<String>,
        /// The secret-store reference that will authenticate, or how git
        /// will (`ssh`, `local`, `git credential helper`).
        auth: String,
    },
    /// `mirror --execute`: one forge took the push (possibly in part).
    MirrorPushed {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        url: String,
        provider: String,
        updated: Vec<String>,
        up_to_date: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        rejected: Vec<String>,
    },
    /// `mirror --execute`: one forge could not be pushed to at all.
    MirrorFailed {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        url: String,
        provider: String,
        message: String,
    },
    /// `mirror`: a repo left out, and why.
    MirrorSkipped {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        reason: String,
    },
    /// `mirror --status`: one forge compared with the checkout.
    MirrorStatus {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        url: String,
        provider: String,
        /// `branch: same | missing | differs`
        branches: Vec<String>,
        tags_missing: u32,
        tags_differ: u32,
    },
    /// `mirror`: the run, in counts.
    MirrorSummary {
        repos: u32,
        remotes: u32,
        planned: u32,
        pushed: u32,
        failed: u32,
        skipped: u32,
        dry_run: bool,
    },
    /// `pr`: the pull request, or with `dry_run` what would be opened.
    PullRequest {
        repo: String,
        head: String,
        base: String,
        title: String,
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        number: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    /// Emitted by `forge_port_schema` (and harness capability probe).
    /// Names the exact four-field D3 intersection + the three
    /// lifecycle methods pinned by V5PROV-2 (D10).
    ForgePortSchema {
        fields: Vec<String>,
        methods: Vec<String>,
        error_classes: Vec<String>,
    },
    /// Capability alias emitted alongside `forge_port_schema` for
    /// harness discoverability; same payload.
    Capability {
        fields: Vec<String>,
        methods: Vec<String>,
        error_classes: Vec<String>,
    },
    /// One summary per repo (streamed by `list`).
    RepoSummary {
        org: String,
        name: String,
        remote_count: usize,
    },
    /// Full repo detail with derived remote providers.
    RepoDetail {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        remotes: Vec<RemoteWire>,
        /// Local metadata (echoed when declared); absent when no
        /// `metadata:` block on the repo entry.
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<RepoMetadataLocal>,
    },
    /// Acknowledgement of a removed repo.
    RepoRemoved { org: String, name: String },
    /// Acknowledgement of an added repo (V5PROV-6). Emitted after
    /// the local entry is written (and, when `create_remote=true`,
    /// after `repo_created`).
    RepoAdded {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        remotes: Vec<RemoteWire>,
    },
    /// Emitted by `repos.add --create_remote true` on successful
    /// `adapter.create_repo` (V5PROV-6). `url` is the first remote.
    RepoCreated {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        url: String,
    },
    /// Emitted by `repos.delete` (V5PROV-7) after the local entry is
    /// dropped. Distinct from `RepoRemoved` (V5REPOS-6) — both mean
    /// local success, but `repos.delete` is the V5PROV-flow method
    /// and callers match on this type.
    RepoDeleted {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
    },
    /// Emitted by `repos.delete --delete_remote true` on successful
    /// `adapter.delete_repo` (V5PROV-7). `url` is the first remote.
    RemoteDeleted {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        url: String,
    },
    /// Per-remote forge metadata snapshot.
    ForgeMetadata {
        url: String,
        default_branch: String,
        description: String,
        archived: bool,
        visibility: String,
    },
    /// Drift report per remote.
    SyncDiff {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        url: String,
        status: String,
        drift: Vec<DriftField>,
        /// Present when `status == "errored"`.
        #[serde(skip_serializing_if = "Option::is_none")]
        error_class: Option<String>,
        /// Snapshot of the four-field shape when the forge read
        /// succeeded. Callers reading a metadata event (V5REPOS-9/10/11
        /// AC1) can match on the `remote` field set.
        #[serde(skip_serializing_if = "Option::is_none")]
        remote: Option<ForgeMetadata>,
    },
    /// Per-remote push success.
    PushRemoteOk { url: String, fields: Vec<String> },
    /// Per-remote push failure. First failure aborts the remaining
    /// remotes per D4.
    PushRemoteError {
        url: String,
        error_class: String,
        message: String,
    },
    /// Final summary after a push run.
    PushSummary {
        succeeded: Vec<String>,
        errored: Vec<PushErrored>,
        aborted: bool,
    },
    /// Error event (typed). Always carries the emitting ticket's
    /// closed error-class set where applicable.
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_class: Option<String>,
        message: String,
    },
    // V5LIFECYCLE-6/7/8/9 events -----------------------------------------
    /// Emitted by `repos.delete` per-provider when privatization succeeds.
    ForgePrivatized {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        provider: String,
        url: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        dry_run: bool,
    },
    /// Emitted by `repos.delete` per-provider when privatization fails.
    PrivatizeError {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        provider: String,
        error_class: String,
        message: String,
    },
    /// Emitted at the end of a successful `repos.delete` flow.
    RepoDismissed {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        privatized_on: Vec<String>,
        already: bool,
    },
    /// Emitted by `repos.purge` per-provider when forge delete succeeds.
    ForgeDeleted {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        provider: String,
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// Emitted by `repos.purge` per-provider on forge error.
    PurgeError {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        provider: String,
        error_class: String,
        message: String,
    },
    /// Emitted at the end of a successful `repos.purge`.
    RepoPurged {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
    },
    /// Emitted by `repos.protect`.
    RepoProtectionSet {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        protected: bool,
    },
    /// Emitted by `repos.init`.
    HyperforgeConfigWritten {
        path: String,
        repo_name: String,
        org: String,
    },
    /// Emitted by `repos.import` per repo that was registered into the
    /// org yaml.
    RepoImported {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        url: String,
    },
    /// Emitted at the end of `repos.import`.
    ImportSummary {
        org: String,
        total: u32,
        added: u32,
        skipped: u32,
    },
    // V5PARITY-3 git transport events.
    CloneDone {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        url: String,
        dest: String,
    },
    FetchDone {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        #[serde(skip_serializing_if = "Option::is_none")]
        remote: Option<String>,
    },
    PullDone {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        remote: String,
        branch: String,
    },
    PushRefsDone {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        remote: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
    RepoStatus {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        upstream: Option<String>,
        ahead: u32,
        behind: u32,
        staged: u32,
        unstaged: u32,
        untracked: u32,
        dirty: bool,
    },
    RepoDirty {
        path: String,
        dirty: bool,
    },
    TransportSet {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        transport: String,
    },
    // V5PARITY-6 lifecycle events.
    RepoRenamed {
        old_ref: RepoRefWire,
        new_ref: RepoRefWire,
    },
    /// A repo was moved to a different org: org-membership relocated
    /// (source org yaml → target org yaml), `.hyperforge/config.toml`
    /// `org` flipped, and remote URLs retargeted. The local
    /// `config.toml` shows dirty after this — the caller commits it.
    RepoMigrated {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        old_org: String,
        new_org: String,
        /// Path to the local checkout whose `.hyperforge/config.toml`
        /// org was flipped, when a checkout was known. `None` when no
        /// `path`/`dir` was supplied (yaml-only migration).
        #[serde(skip_serializing_if = "Option::is_none")]
        local_path: Option<String>,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        dry_run: bool,
    },
    DefaultBranchSet {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        branch: String,
    },
    ArchivedSet {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        archived: bool,
    },
    // V5PARITY-4 analytics events.
    RepoSizeSummary {
        path: String,
        bytes: u64,
        file_count: u64,
    },
    RepoLocSummary {
        path: String,
        by_language: BTreeMap<String, u64>,
        total: u64,
    },
    LargeFile {
        path: String,
        size: u64,
    },
    LargeFilesSummary {
        path: String,
        threshold_bytes: u64,
        count: u64,
    },
    // V5PARITY-5 SSH events.
    RepoSshKeySet {
        path: String,
        key: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        org: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        persisted: bool,
    },
    // V5PARITY-36: cross-forge migrate / retire events.
    MigrateStarted {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        source_url: String,
        dest_provider: String,
        dest_org: String,
        dest_name: String,
    },
    ForgeMigrated {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        dest_url: String,
    },
    MigrateDone {
        source_ref: RepoRefWire,
        dest_ref: RepoRefWire,
        retired: bool,
        archived: bool,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        dry_run: bool,
    },
    MigrateFailed {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        stage: String, // precheck | source_resolve | forge_migrate | v5_register | retire | archive
        message: String,
    },
    // V5PARITY-34: sync_config event.
    ConfigSynced {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        mode: String, // "push" | "pull"
        local_path: String,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        changed: bool,
    },
    /// V5PARITY-35: per-repo `forges` scope updated.
    ForgesSet {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        /// `null` when the field is unset (legacy unscoped behavior).
        forges: Option<Vec<String>>,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        changed: bool,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        dry_run: bool,
        /// Path to the per-repo file if it was updated, else `None`.
        #[serde(skip_serializing_if = "Option::is_none")]
        local_path: Option<String>,
    },
    // V5PARITY-25: adopt-existing-checkout events.
    /// `repos.register` succeeded — the local checkout is now tracked.
    RepoRegistered {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        path: String,
        remotes: Vec<RemoteWire>,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        init_done: bool,
    },
    /// `repos.register` found an existing entry under the same name
    /// with different remotes — refuses to overwrite. Caller resolves
    /// manually (e.g. `repos.add_remote` to merge).
    RepoConflict {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        existing_remotes: Vec<String>,
        observed_remotes: Vec<String>,
    },
    // HYPE-6: redirect-detect + heal (repos.doctor / repos.sync --heal).
    /// Per-repo doctor verdict: how the registry's owner compares to the
    /// forge's canonical owner. `verdict` ∈ {clean, diverged, unknown}.
    /// `clean` includes owner-aliases (HYPE-5) — an aliased owner is not a
    /// divergence.
    RepoDoctorEntry {
        #[serde(rename = "ref")]
        reference: RepoRefWire,
        declared_owner: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        canonical_owner: Option<String>,
        verdict: String,
        /// HYPERFORGE-58: why the verdict is `unknown` (no remote that can
        /// answer, or the forge's error), so an unknown is never silent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Emitted when `--heal` repaired a divergence through `migrate_one`.
    RepoHealed {
        old_full_name: String,
        new_full_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        local_path: Option<String>,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        dry_run: bool,
        /// HYPERFORGE-58: the canonical org already had the repo; the
        /// stale entry is dropped rather than duplicated.
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        already_in_target: bool,
    },
    /// Final `doctor`/`--heal` summary.
    RepoDoctorSummary {
        org: String,
        checked: u32,
        diverged: u32,
        healed: u32,
        /// Path to the written rename map (old→new full_name), when
        /// `--heal` applied at least one rename.
        #[serde(skip_serializing_if = "Option::is_none")]
        renames_path: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RepoRefWire {
    pub org: String,
    pub name: String,
}

impl From<&RepoRef> for RepoRefWire {
    fn from(r: &RepoRef) -> Self {
        Self {
            org: r.org.as_str().to_string(),
            name: r.name.as_str().to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RemoteWire {
    pub url: String,
    pub provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DriftField {
    pub field: String,
    pub local: serde_json::Value,
    pub remote: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PushErrored {
    pub url: String,
    pub error_class: String,
    pub message: String,
}

// ---------------------------------------------------------------------
// Service.
// ---------------------------------------------------------------------

/// Repos namespace. Methods attached here implement V5REPOS-{2..14}.
#[derive(Clone, Default)]
pub struct ReposService {
    config_dir: PathBuf,
}

impl ReposService {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            config_dir: PathBuf::new(),
        }
    }

    #[must_use]
    pub const fn with_config_dir(config_dir: PathBuf) -> Self {
        Self { config_dir }
    }
}

// ---------------------------------------------------------------------
// Provider derivation (V5REPOS-12).
// ---------------------------------------------------------------------

// V5LIFECYCLE-3: relocated to `crate::v5::ops::repo::derive_provider`.
// Re-exported here so existing callsites in this module and
// `workspaces.rs` keep their short name without reintroducing a
// duplicate implementation.
pub(crate) use crate::v5::ops::repo::derive_provider;

fn remote_to_wire(
    remote: &Remote,
    provider_map: &BTreeMap<DomainName, ProviderKind>,
) -> Result<RemoteWire, String> {
    let provider = derive_provider(remote, provider_map)?;
    Ok(RemoteWire {
        url: remote.url.as_str().to_string(),
        provider: match provider {
            ProviderKind::Github => "github",
            ProviderKind::Codeberg => "codeberg",
            ProviderKind::Gitlab => "gitlab",
        }
        .to_string(),
    })
}

// ---------------------------------------------------------------------
// Repo-entry lookup helpers.
// ---------------------------------------------------------------------

fn find_repo_mut<'a>(org: &'a mut OrgConfig, name: &str) -> Option<&'a mut OrgRepo> {
    org.repos.iter_mut().find(|r| r.name.as_str() == name)
}

fn find_repo<'a>(org: &'a OrgConfig, name: &str) -> Option<&'a OrgRepo> {
    org.repos.iter().find(|r| r.name.as_str() == name)
}

// ---------------------------------------------------------------------
// Param parsing helpers — synapse passes bools/structs as strings.
// ---------------------------------------------------------------------

fn to_bool(v: &Value, default: bool) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::String(s) => matches!(s.to_lowercase().as_str(), "true" | "1" | "yes" | "on"),
        Value::Null => default,
        _ => default,
    }
}

fn parse_remotes(raw: &Value) -> Result<Vec<Remote>, String> {
    let v = unwrap_json_string(raw);
    let arr = v
        .as_array()
        .ok_or_else(|| format!("remotes must be a JSON array, got: {v}"))?;
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let r: Remote = serde_json::from_value(entry.clone())
            .map_err(|e| format!("invalid remote entry {entry}: {e}"))?;
        if r.url.as_str().is_empty() {
            return Err("remote url is empty".to_string());
        }
        out.push(r);
    }
    Ok(out)
}

fn parse_remote(raw: &Value) -> Result<Remote, String> {
    let v = unwrap_json_string(raw);
    let r: Remote = serde_json::from_value(v.clone())
        .map_err(|e| format!("invalid remote {v}: {e}"))?;
    if r.url.as_str().is_empty() {
        return Err("remote url is empty".to_string());
    }
    Ok(r)
}

fn parse_fields(raw: &Value) -> Result<MetadataFields, String> {
    let v = unwrap_json_string(raw);
    let map = v
        .as_object()
        .ok_or_else(|| format!("fields must be a JSON object, got: {v}"))?;
    let mut out = MetadataFields::new();
    for (k, val) in map {
        let kind = match k.as_str() {
            "default_branch" => DriftFieldKind::DefaultBranch,
            "description" => DriftFieldKind::Description,
            "archived" => DriftFieldKind::Archived,
            "visibility" => DriftFieldKind::Visibility,
            other => {
                return Err(format!(
                    "unsupported field '{other}'; allowed: default_branch, description, archived, visibility"
                ))
            }
        };
        out.insert(kind, val.clone());
    }
    Ok(out)
}

/// Synapse wraps structured params as `Value::String` of raw JSON.
/// Parse-if-string so callers receive the typed shape.
fn unwrap_json_string(raw: &Value) -> Value {
    if let Value::String(s) = raw {
        if let Ok(inner) = serde_json::from_str::<Value>(s) {
            return inner;
        }
    }
    raw.clone()
}

// ---------------------------------------------------------------------
// Error helpers.
// ---------------------------------------------------------------------

fn cfg_error_event(err: ConfigError) -> RepoEvent {
    RepoEvent::Error {
        code: Some("config_error".into()),
        error_class: None,
        message: err.to_string(),
    }
}

fn not_found_event(msg: impl Into<String>) -> RepoEvent {
    RepoEvent::Error {
        code: Some("not_found".into()),
        error_class: Some("not_found".into()),
        message: msg.into(),
    }
}

fn validation_event(msg: impl Into<String>) -> RepoEvent {
    RepoEvent::Error {
        code: Some("validation".into()),
        error_class: None,
        message: msg.into(),
    }
}

// ---------------------------------------------------------------------
// Org migration — shared between repos.migrate_org and
// workspaces.migrate_org (one implementation, no duplication).
// ---------------------------------------------------------------------

/// Retarget a single remote URL's org segment `old_org` → `new_org`.
///
/// Handles both ssh (`host:OLD_ORG/name`) and https
/// (`host/OLD_ORG/name`) forms by replacing only the path-anchored
/// occurrences `:OLD_ORG/` and `/OLD_ORG/`, so the host (which never
/// matches either pattern unless it is literally followed by `/OLD/`)
/// is left intact.
fn retarget_remote_url(url: &str, old_org: &str, new_org: &str) -> String {
    url.replace(&format!(":{old_org}/"), &format!(":{new_org}/"))
        .replace(&format!("/{old_org}/"), &format!("/{new_org}/"))
}

/// Outcome of a successful per-repo migration.
pub(crate) struct MigrateOutcome {
    /// Local checkout whose `.hyperforge/config.toml` org was flipped,
    /// when a dir was known and the config existed.
    pub(crate) local_path: Option<String>,
    /// The rename this migration records (HYPE-6): `old_org/name`.
    pub(crate) old_full_name: String,
    /// `new_org/name` — where the repo now lives.
    pub(crate) new_full_name: String,
    /// HYPERFORGE-58: the target org already had this repo, so the move
    /// drops the stale source entry instead of adding a duplicate.
    pub(crate) already_in_target: bool,
}

/// Move ONE repo from org `org` to org `new_org`:
///
/// 1. Remove the `OrgRepo` from the source org's `repos` vec, retarget
///    its remotes, push it into the target org's `repos` vec, save BOTH
///    org yamls.
/// 2. When `dir_opt` is a known local checkout: flip its
///    `.hyperforge/config.toml` `org` (force-write) and `set_remote_url`
///    for each retargeted remote.
/// 3. Rewrite every workspace yaml ref `old_org/name` → `new_org/name`.
///
/// `provider` is the forge whose owner actually changed (HYPE-6): only
/// remotes on that provider are retargeted, so a github owner-rename can
/// never rewrite a codeberg (or any other forge) URL — those are left
/// byte-identical.
///
/// The TARGET org is created if it is not already registered (HYPE-6): an
/// owner-rename lands under the canonical org even when that org was never
/// bootstrapped. On `dry`, all computation runs but NO writes happen (org
/// yamls, config.toml, git remotes, workspaces).
pub(crate) fn migrate_one(
    config_dir: &std::path::Path,
    org: &str,
    name: &str,
    new_org: &str,
    provider: ProviderKind,
    dir_opt: Option<&std::path::Path>,
    dry: bool,
) -> Result<MigrateOutcome, String> {
    let loaded = crate::v5::ops::state::load_all(config_dir)
        .map_err(|e| format!("config_error: {e}"))?;
    let provider_map = &loaded.global.provider_map;

    let Some(source) = loaded.orgs.get(&OrgName::from(org)) else {
        return Err(format!("not_found: org '{org}' not found"));
    };
    let Some(existing_repo) = crate::v5::ops::state::find_repo(source, name) else {
        return Err(format!("not_found: repo '{name}' not found under org '{org}'"));
    };
    // HYPE-6: the target org is created (not required to pre-exist). A
    // repo that redirects to a not-yet-bootstrapped org still heals; the
    // new org inherits the source org's forge/credential blocks.
    let target_existing = loaded.orgs.get(&OrgName::from(new_org)).cloned();
    let already_in_target = target_existing
        .as_ref()
        .is_some_and(|t| t.repos.iter().any(|r| r.name.as_str() == name));

    // Compute the retargeted OrgRepo. HYPE-6: retarget ONLY remotes on the
    // migrating provider; every other forge's URL stays byte-identical.
    let mut moved = existing_repo.clone();
    for r in &mut moved.remotes {
        let this_provider = crate::v5::ops::repo::derive_provider(r, provider_map).ok();
        if this_provider == Some(provider) {
            let new_url = retarget_remote_url(r.url.as_str(), org, new_org);
            r.url = RemoteUrl::from(new_url.as_str());
        }
    }

    let old_full_name = format!("{org}/{name}");
    let new_full_name = format!("{new_org}/{name}");

    // Determine the local-checkout side-effects up front so dry_run can
    // preview the path without performing them.
    let mut local_path: Option<String> = None;
    // HYPERFORGE-58: any git checkout counts, with or without
    // `.hyperforge/config.toml` (which only some checkouts carry).
    if let Some(dir) = dir_opt {
        if dir.join(".git").exists() {
            local_path = Some(dir.display().to_string());
        }
    }

    if dry {
        return Ok(MigrateOutcome { local_path, old_full_name, new_full_name, already_in_target });
    }

    // --- Writes below this line. ---

    // 1. Move membership: remove from source, push into target (creating
    //    the target org if it did not exist).
    let mut source_updated = source.clone();
    source_updated.repos.retain(|r| r.name.as_str() != name);
    let mut target_updated = target_existing.unwrap_or_else(|| OrgConfig {
        name: OrgName::from(new_org),
        forge: source.forge.clone(),
        mirrors: source.mirrors.clone(),
        repos: Vec::new(),
    });
    if !already_in_target {
        target_updated.repos.push(moved.clone());
    }

    let orgs_dir = config_dir.join("orgs");
    crate::v5::ops::state::save_org(&orgs_dir, &source_updated)
        .map_err(|e| format!("config_error: {e}"))?;
    crate::v5::ops::state::save_org(&orgs_dir, &target_updated)
        .map_err(|e| format!("config_error: {e}"))?;

    // 2. Local checkout: flip config.toml org (when the checkout has one)
    //    and retarget git remotes (always — HYPERFORGE-58: a checkout
    //    without config.toml was silently left on the old owner).
    if let Some(dir) = dir_opt {
        if let Ok(Some(mut cfg)) = crate::v5::ops::fs::read_hyperforge_config(dir) {
            cfg.org = OrgName::from(new_org);
            crate::v5::ops::fs::write_hyperforge_config(dir, &cfg, true)
                .map_err(|e| format!("config_error: writing .hyperforge/config.toml: {e}"))?;
        }
        // HYPE-6: retarget the local git remotes by their ACTUAL
        // `.git/config` names (not provider convention), and ONLY the
        // remotes on the migrating provider. On the split-convention
        // fleet a github remote may be named `origin`, `github`, or
        // anything else; a codeberg remote named `origin` must be left
        // untouched. A failed set-url is an error, not silence.
        if let Ok(named) = read_named_remotes(dir) {
            for (remote_name, url) in named {
                let probe = crate::v5::config::Remote {
                    url: RemoteUrl::from(url.as_str()),
                    provider: None,
                };
                let this_provider =
                    crate::v5::ops::repo::derive_provider(&probe, provider_map).ok();
                if this_provider != Some(provider) {
                    continue; // other forge — leave byte-identical
                }
                let retargeted = retarget_remote_url(&url, org, new_org);
                if retargeted != url {
                    crate::v5::ops::git::set_remote_url(dir, &remote_name, &retargeted)
                        .map_err(|e| format!("git_error: set-url {remote_name}: {e}"))?;
                }
            }
        }
    }

    // 3. Rewrite workspace refs old_org/name → new_org/name.
    let ws_dir = config_dir.join("workspaces");
    if ws_dir.is_dir() {
        if let Ok(all_ws) = crate::v5::ops::state::load_workspaces(&ws_dir) {
            for (_, mut ws) in all_ws {
                let mut changed = false;
                for entry in &mut ws.repos {
                    match entry {
                        crate::v5::config::WorkspaceRepo::Shorthand(s) => {
                            if *s == format!("{org}/{name}") {
                                *s = format!("{new_org}/{name}");
                                changed = true;
                            }
                        }
                        crate::v5::config::WorkspaceRepo::Object { reference, .. } => {
                            if reference.org.as_str() == org && reference.name.as_str() == name {
                                reference.org = OrgName::from(new_org);
                                changed = true;
                            }
                        }
                    }
                }
                if changed {
                    let _ = crate::v5::ops::state::save_workspace(&ws_dir, &ws);
                }
            }
        }
    }

    Ok(MigrateOutcome { local_path, old_full_name, new_full_name, already_in_target })
}

/// Read the checkout's remotes as `(name, url)` pairs straight from
/// `.git/config` via git2 (HYPE-6). Unlike [`collect_all_remotes`] this
/// preserves the actual remote NAMES, which `migrate_one` needs to
/// retarget the right remote on the split-convention fleet.
fn read_named_remotes(dir: &std::path::Path) -> Result<Vec<(String, String)>, ()> {
    use git2::Repository;
    let repo = Repository::open(dir).map_err(|_| ())?;
    let names = repo.remotes().map_err(|_| ())?;
    let mut out: Vec<(String, String)> = Vec::new();
    for name in names.iter().flatten() {
        if let Ok(remote) = repo.find_remote(name) {
            if let Some(url) = remote.url() {
                out.push((name.to_string(), url.to_string()));
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// HYPE-6: redirect-detect + heal (repos.doctor / repos.sync --heal).
// ---------------------------------------------------------------------

/// How a repo's registered owner compares to the forge's canonical owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoctorVerdict {
    /// Same canonical identity — includes owner-aliases (HYPE-5), which are
    /// NOT a divergence.
    Clean,
    /// The forge's canonical owner is a genuinely different identity.
    Diverged,
}

impl DoctorVerdict {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Diverged => "diverged",
        }
    }
}

/// Pure divergence classifier (HYPE-6): compares the registry's declared
/// owner to the forge's canonical owner **through `canonical_owner`**, so an
/// aliased owner is `Clean` and only a real owner change is `Diverged`.
pub(crate) fn doctor_verdict(
    global: &GlobalConfig,
    declared_owner: &str,
    forge_owner: &str,
) -> DoctorVerdict {
    if global.same_owner(declared_owner, forge_owner) {
        DoctorVerdict::Clean
    } else {
        DoctorVerdict::Diverged
    }
}

/// Write the rename map (old_full_name → new_full_name) to a JSON file in
/// the config dir (HYPE-6). Uses serde_json (not serde_yaml — the state
/// layer's format is untouched here; this is an operator report artifact).
fn write_rename_map(
    config_dir: &std::path::Path,
    renames: &[(String, String)],
) -> std::io::Result<std::path::PathBuf> {
    let map: std::collections::BTreeMap<&str, &str> =
        renames.iter().map(|(o, n)| (o.as_str(), n.as_str())).collect();
    let body = serde_json::to_string_pretty(&map).unwrap_or_else(|_| "{}".to_string());
    let path = config_dir.join("doctor-renames.json");
    std::fs::write(&path, body)?;
    Ok(path)
}

/// Resolve one repo's canonical owner on the forge, report the verdict, and
/// (when `do_heal` and the verdict is `Diverged`) repair it through
/// `migrate_one` — the SAME migration primitive, now provider-scoped
/// (HYPE-6). Returns the events to stream plus the rename it recorded, if
/// any. This is the shared core of `repos.doctor` and `repos.sync --heal`.
/// The ONLY network call in the doctor/heal path lives here
/// (`canonical_owner_on_forge`); nothing ambient reaches the forge.
#[allow(clippy::too_many_arguments)]
async fn doctor_one(
    config_dir: &std::path::Path,
    org: &str,
    org_cfg: &OrgConfig,
    repo: &OrgRepo,
    global: &GlobalConfig,
    resolver: &dyn SecretResolver,
    do_heal: bool,
    dir_opt: Option<&std::path::Path>,
    dry: bool,
) -> (Vec<RepoEvent>, Option<(String, String)>) {
    let mut events: Vec<RepoEvent> = Vec::new();
    let name = repo.name.as_str();
    let reference = RepoRefWire { org: org.to_string(), name: name.to_string() };

    // Pick a remote that can answer canonical identity (only github
    // implements it today); no such remote → unknown, never migrate.
    let Some(remote) = crate::v5::ops::repo::canonical_remote_in_scope(repo, &global.provider_map)
    else {
        events.push(RepoEvent::RepoDoctorEntry {
            reference,
            declared_owner: org.to_string(),
            canonical_owner: None,
            verdict: "unknown".into(),
            reason: Some(
                if crate::v5::ops::repo::all_remotes_excluded(repo, &global.provider_map) {
                    "excluded: the repo's `forges` scope admits none of its remotes".into()
                } else {
                    "no remote on a forge that can report its owner".into()
                },
            ),
        });
        return (events, None);
    };
    let provider = crate::v5::ops::repo::derive_provider(remote, &global.provider_map).ok();
    let repo_ref = RepoRef {
        org: OrgName::from(org),
        name: RepoName::from(name),
    };
    let token_ref = provider
        .and_then(|p| crate::v5::ops::repo::token_ref_scoped(org_cfg, p))
        .map(str::to_string);
    let fallback = provider.map(crate::v5::ops::repo::provider_default_token_ref);

    let forge_owner = crate::v5::ops::repo::canonical_owner_on_forge(
        remote,
        &repo_ref,
        &global.provider_map,
        resolver,
        token_ref.as_deref(),
        fallback,
    )
    .await
    .map_err(|e| e.to_string());
    doctor_apply(config_dir, org, name, global, forge_owner, provider, do_heal, dir_opt, dry)
}

/// HYPERFORGE-58: the decision half of `doctor_one`, given the forge's
/// answer (`Err` = unresolved: network failure or a forge that can't say).
/// Unresolved is `unknown` and never migrates; `Diverged` heals only with
/// `do_heal`, and with `dry` reports the repair without writing.
#[allow(clippy::too_many_arguments)]
fn doctor_apply(
    config_dir: &std::path::Path,
    org: &str,
    name: &str,
    global: &GlobalConfig,
    forge_owner: Result<String, String>,
    provider: Option<ProviderKind>,
    do_heal: bool,
    dir_opt: Option<&std::path::Path>,
    dry: bool,
) -> (Vec<RepoEvent>, Option<(String, String)>) {
    let mut events: Vec<RepoEvent> = Vec::new();
    let reference = RepoRefWire { org: org.to_string(), name: name.to_string() };
    let forge_owner = match forge_owner {
        Ok(o) => o,
        Err(reason) => {
            events.push(RepoEvent::RepoDoctorEntry {
                reference,
                declared_owner: org.to_string(),
                canonical_owner: None,
                verdict: "unknown".into(),
                reason: Some(reason),
            });
            return (events, None);
        }
    };
    let verdict = doctor_verdict(global, org, &forge_owner);
    events.push(RepoEvent::RepoDoctorEntry {
        reference,
        declared_owner: org.to_string(),
        canonical_owner: Some(forge_owner.clone()),
        verdict: verdict.as_str().to_string(),
        reason: None,
    });
    if verdict != DoctorVerdict::Diverged || !do_heal {
        return (events, None);
    }
    // Heal to the CANONICAL form of the forge's answer.
    let canonical = global.canonical_owner(&forge_owner);
    let migrating_provider = provider.unwrap_or(ProviderKind::Github);
    match migrate_one(config_dir, org, name, &canonical, migrating_provider, dir_opt, dry) {
        Ok(outcome) => {
            let rename = (outcome.old_full_name.clone(), outcome.new_full_name.clone());
            events.push(RepoEvent::RepoHealed {
                old_full_name: outcome.old_full_name,
                new_full_name: outcome.new_full_name,
                local_path: outcome.local_path,
                dry_run: dry,
                already_in_target: outcome.already_in_target,
            });
            (events, Some(rename))
        }
        Err(msg) => {
            let (code, body) = msg
                .split_once(": ")
                .map_or(("validation", msg.as_str()), |(c, b)| (c, b));
            events.push(RepoEvent::Error {
                code: Some(code.to_string()),
                error_class: None,
                message: body.to_string(),
            });
            (events, None)
        }
    }
}

// ---------------------------------------------------------------------
// Activation.
// ---------------------------------------------------------------------

/// Repos CRUD + `ForgePort` surface.
#[plexus_macros::activation(
    trait = "ReposApi",
    methods = marked,
    namespace = "repos",
    description = "Repos CRUD + ForgePort",
    crate_path = "plexus_core"
)]
impl ReposService {
    /// V5REPOS-2 / V5PROV-2 capability surface: announces the four D3
    /// fields, the five original error classes plus `conflict` and
    /// `unsupported_visibility`, and the seven trait method names
    /// (four metadata + three lifecycle).
    #[plexus_macros::method]
    pub async fn forge_port_schema(
        &self,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        stream! {
            let fields: Vec<String> = DriftFieldKind::all()
                .iter()
                .map(|k| k.as_str().to_string())
                .collect();
            let methods = vec![
                "create_repo".to_string(),
                "delete_repo".to_string(),
                "read_metadata".to_string(),
                "repo_exists".to_string(),
                "write_metadata".to_string(),
            ];
            let error_classes = vec![
                "auth".to_string(),
                "conflict".to_string(),
                "network".to_string(),
                "not_found".to_string(),
                "rate_limited".to_string(),
                "unsupported_field".to_string(),
                "unsupported_visibility".to_string(),
            ];
            yield RepoEvent::ForgePortSchema {
                fields: fields.clone(),
                methods: methods.clone(),
                error_classes: error_classes.clone(),
            };
            yield RepoEvent::Capability {
                fields,
                methods,
                error_classes,
            };
        }
    }

    /// V5REPOS-3: stream one `RepoSummary` per repo in the org.
    #[plexus_macros::method(params(org = "Org name"))]
    pub async fn list(&self, org: String) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir: Result<PathBuf, String> = Ok(self.config_dir.clone());
        stream! {
            let dir = match config_dir {
                Ok(d) => d,
                Err(e) => { yield RepoEvent::Error { code: Some("config_error".into()), error_class: None, message: e }; return; }
            };
            if org.is_empty() {
                yield validation_event("missing required parameter 'org'");
                return;
            }
            let orgs_dir = dir.join("orgs");
            let orgs = match load_orgs(&orgs_dir) {
                Ok(o) => o,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(org_cfg) = orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found"));
                return;
            };
            let mut entries: Vec<&OrgRepo> = org_cfg.repos.iter().collect();
            entries.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
            for repo in entries {
                yield RepoEvent::RepoSummary {
                    org: org_cfg.name.as_str().to_string(),
                    name: repo.name.as_str().to_string(),
                    remote_count: repo.remotes.len(),
                };
            }
        }
    }

    /// V5REPOS-4: full `RepoDetail` including derived providers.
    #[plexus_macros::method(params(org = "Org name", name = "Repo name"))]
    pub async fn get(
        &self,
        org: String,
        name: String,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir: Result<PathBuf, String> = Ok(self.config_dir.clone());
        stream! {
            let dir = match config_dir {
                Ok(d) => d,
                Err(e) => { yield RepoEvent::Error { code: Some("config_error".into()), error_class: None, message: e }; return; }
            };
            if org.is_empty() {
                yield validation_event("missing required parameter 'org'");
                return;
            }
            if name.is_empty() {
                yield validation_event("missing required parameter 'name'");
                return;
            }
            let loaded = match load_all(&dir) {
                Ok(l) => l,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(org_cfg) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found"));
                return;
            };
            let Some(repo) = find_repo(org_cfg, &name) else {
                yield not_found_event(format!("repo '{name}' not found under org '{org}'"));
                return;
            };
            match repo_detail_event(org, name, repo, &loaded.global.provider_map) {
                Ok(ev) => { yield ev; }
                Err(msg) => { yield validation_event(msg); }
            }
        }
    }

    /// V5REPOS-5 + V5PROV-6: register a new repo with initial remotes.
    ///
    /// When `create_remote=true` is set, the adapter's `create_repo`
    /// is called after the local entry is written. The pinned order
    /// (per V5PROV-1 R2): validate → write local → call `repo_exists`
    /// (conflict if present) → call `create_repo` (on failure, roll
    /// back local entry) → emit `repo_created` + `repo_added`.
    ///
    /// When `create_remote=false` (default), the method is backward
    /// compatible with V5REPOS-5 and emits `repo_detail` + `repo_added`
    /// after writing the local entry (no forge contact).
    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        remotes = "JSON array of remotes",
        create_remote = "Also create the repo on the remote forge (default false)",
        visibility = "Visibility for `create_remote`: public | private | internal (default private)",
        description = "Description passed to `create_remote` (default empty)",
        dry_run = "Preview without writing"
    ))]
    pub async fn add(
        &self,
        org: String,
        name: String,
        remotes: Value,
        create_remote: Option<Value>,
        visibility: Option<String>,
        description: Option<String>,
        dry_run: Option<Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir: Result<PathBuf, String> = Ok(self.config_dir.clone());
        stream! {
            let dir = match config_dir {
                Ok(d) => d,
                Err(e) => { yield RepoEvent::Error { code: Some("config_error".into()), error_class: None, message: e }; return; }
            };
            let dry = dry_run.as_ref().is_some_and(|v| to_bool(v, false));
            let forge_create = create_remote.as_ref().is_some_and(|v| to_bool(v, false));
            if org.is_empty() {
                yield validation_event("missing required parameter 'org'");
                return;
            }
            if name.is_empty() {
                yield validation_event("missing required parameter 'name'");
                return;
            }
            let parsed_remotes = match parse_remotes(&remotes) {
                Ok(r) => r,
                Err(e) => { yield validation_event(e); return; }
            };
            if parsed_remotes.is_empty() {
                yield validation_event("remotes must contain at least one entry");
                return;
            }
            // Parse visibility. On `create_remote=false` the value is
            // still parsed for validation — a garbage input fails
            // fast rather than being silently ignored.
            let vis_raw = visibility
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("private");
            let vis = match ProviderVisibility::parse(vis_raw) {
                Ok(v) => v,
                Err(e) => { yield validation_event(e); return; }
            };
            let desc = description.unwrap_or_default();

            let loaded = match load_all(&dir) {
                Ok(l) => l,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let provider_map = loaded.global.provider_map.clone();
            // Validate every remote's provider derives cleanly.
            for r in &parsed_remotes {
                if let Err(e) = derive_provider(r, &provider_map) {
                    yield validation_event(e);
                    return;
                }
            }
            let Some(existing) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found"));
                return;
            };
            if existing.repos.iter().any(|r| r.name.as_str() == name) {
                yield validation_event(format!(
                    "repo '{name}' already exists under org '{org}'"
                ));
                return;
            }
            let mut updated = existing.clone();
            updated.repos.push(OrgRepo {
                name: RepoName::from(name.as_str()),
                remotes: parsed_remotes.clone(),
                forges: None,
                metadata: None,
            });
            let orgs_dir = dir.join("orgs");
            if !dry {
                if let Err(e) = save_org(&orgs_dir, &updated) {
                    yield cfg_error_event(e);
                    return;
                }
            }
            // ---------- create_remote flow ----------
            if forge_create {
                let first = &parsed_remotes[0];
                let provider = match derive_provider(first, &provider_map) {
                    Ok(p) => p,
                    Err(e) => {
                        // Unreachable: we already validated above, but
                        // defend defensively + roll back.
                        if !dry {
                            let rolled_back = existing.clone();
                            let _ = save_org(&orgs_dir, &rolled_back);
                        }
                        yield validation_event(e);
                        return;
                    }
                };
                let repo_ref = RepoRef {
                    org: OrgName::from(org.as_str()),
                    name: RepoName::from(name.as_str()),
                };
                let repo_ref_wire = RepoRefWire::from(&repo_ref);
                let url_s = first.url.as_str().to_string();

                if dry {
                    // Dry run emits the success event stream without
                    // any forge or disk contact.
                    yield RepoEvent::RepoCreated {
                        reference: repo_ref_wire.clone(),
                        url: url_s,
                    };
                    match repo_detail_event(
                        org.clone(),
                        name.clone(),
                        updated.repos.last().unwrap(),
                        &provider_map,
                    ) {
                        Ok(ev) => yield ev,
                        Err(msg) => { yield validation_event(msg); return; }
                    }
                    // And the RepoAdded ack.
                    let wires: Result<Vec<RemoteWire>, String> = parsed_remotes
                        .iter()
                        .map(|r| remote_to_wire(r, &provider_map))
                        .collect();
                    match wires {
                        Ok(ws) => yield RepoEvent::RepoAdded {
                            reference: repo_ref_wire,
                            remotes: ws,
                        },
                        Err(msg) => yield validation_event(msg),
                    }
                    return;
                }

                // V5LIFECYCLE-4: route through ops::repo wrappers.
                let resolver = YamlSecretStore::new(&dir);
                let token_ref = crate::v5::ops::repo::token_ref_for(existing);
            let fallback_token_ref = Some(crate::v5::ops::repo::default_token_ref_for(existing));
                let _ = provider; // provider is still derived for logging but we no longer need the adapter handle here
                match crate::v5::ops::repo::exists_on_forge(
                    first, &repo_ref, &loaded.global.provider_map, &resolver, token_ref, fallback_token_ref.clone(),
                ).await {
                    Ok(true) => {
                        let rolled_back = existing.clone();
                        if let Err(e) = save_org(&orgs_dir, &rolled_back) {
                            yield cfg_error_event(e);
                        }
                        yield RepoEvent::Error {
                            code: Some("conflict".into()),
                            error_class: Some("conflict".into()),
                            message: format!("repo '{}/{}' already exists on remote", org, name),
                        };
                        return;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        let rolled_back = existing.clone();
                        if let Err(save_err) = save_org(&orgs_dir, &rolled_back) {
                            yield cfg_error_event(save_err);
                        }
                        yield RepoEvent::Error {
                            code: Some(e.class.as_str().into()),
                            error_class: Some(e.class.as_str().into()),
                            message: format!("repo_exists probe failed: {}", e.message),
                        };
                        return;
                    }
                }
                match crate::v5::ops::repo::create_on_forge(
                    first, &repo_ref, vis, &desc, &loaded.global.provider_map, &resolver, token_ref, fallback_token_ref.clone(),
                ).await {
                    Ok(()) => {
                        yield RepoEvent::RepoCreated {
                            reference: repo_ref_wire.clone(),
                            url: url_s,
                        };
                    }
                    Err(e) => {
                        // Roll back local write on forge error.
                        let rolled_back = existing.clone();
                        if let Err(save_err) = save_org(&orgs_dir, &rolled_back) {
                            yield cfg_error_event(save_err);
                        }
                        yield RepoEvent::Error {
                            code: Some(e.class.as_str().into()),
                            error_class: Some(e.class.as_str().into()),
                            message: e.message,
                        };
                        return;
                    }
                }
            }

            // Success: emit RepoDetail (V5REPOS-5 backward compat) +
            // RepoAdded (V5PROV-6 ack).
            let new_repo = updated.repos.last().unwrap();
            let repo_ref_wire = RepoRefWire {
                org: org.clone(),
                name: name.clone(),
            };
            let wires: Result<Vec<RemoteWire>, String> = parsed_remotes
                .iter()
                .map(|r| remote_to_wire(r, &provider_map))
                .collect();
            match repo_detail_event(org, name, new_repo, &provider_map) {
                Ok(ev) => yield ev,
                Err(msg) => { yield validation_event(msg); return; }
            }
            match wires {
                Ok(ws) => yield RepoEvent::RepoAdded {
                    reference: repo_ref_wire,
                    remotes: ws,
                },
                Err(msg) => yield validation_event(msg),
            }
        }
    }

    /// V5REPOS-6: drop the entry. `delete_remote=true` calls the
    /// adapter(s) first — any adapter failure aborts and leaves local.
    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        delete_remote = "Forge-side delete (default false)",
        dry_run = "Preview without writing"
    ))]
    pub async fn remove(
        &self,
        org: String,
        name: String,
        delete_remote: Option<Value>,
        dry_run: Option<Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir: Result<PathBuf, String> = Ok(self.config_dir.clone());
        stream! {
            let dir = match config_dir {
                Ok(d) => d,
                Err(e) => { yield RepoEvent::Error { code: Some("config_error".into()), error_class: None, message: e }; return; }
            };
            let dry = dry_run.as_ref().is_some_and(|v| to_bool(v, false));
            let forge_delete = delete_remote.as_ref().is_some_and(|v| to_bool(v, false));

            if org.is_empty() || name.is_empty() {
                yield validation_event("missing required parameter 'org' or 'name'");
                return;
            }
            let loaded = match load_all(&dir) {
                Ok(l) => l,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(existing) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found"));
                return;
            };
            if find_repo(existing, &name).is_none() {
                yield not_found_event(format!("repo '{name}' not found under org '{org}'"));
                return;
            }
            if forge_delete {
                // Resolve provider for every remote first; any derivation
                // failure aborts before any forge call.
                let provider_map = &loaded.global.provider_map;
                let repo = find_repo(existing, &name).unwrap();
                for r in &repo.remotes {
                    if let Err(e) = derive_provider(r, provider_map) {
                        yield validation_event(e);
                        return;
                    }
                }
                // Forge-side delete not implemented at the metadata
                // trait in v1 scope. Treat as adapter failure so the
                // local entry is preserved (per V5REPOS-6 AC4).
                yield RepoEvent::Error {
                    code: Some("adapter_error".into()),
                    error_class: Some("unsupported_field".into()),
                    message: "delete_remote=true requires forge-side delete; adapter capability not available in v1 ForgePort (local entry preserved)".to_string(),
                };
                return;
            }
            if !dry {
                let mut updated = existing.clone();
                updated.repos.retain(|r| r.name.as_str() != name);
                let orgs_dir = dir.join("orgs");
                if let Err(e) = save_org(&orgs_dir, &updated) {
                    yield cfg_error_event(e);
                    return;
                }
            }
            yield RepoEvent::RepoRemoved { org, name };
        }
    }

    /// V5REPOS-7: append a remote.
    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        remote = "JSON remote object",
        dry_run = "Preview without writing"
    ))]
    pub async fn add_remote(
        &self,
        org: String,
        name: String,
        remote: Value,
        dry_run: Option<Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir: Result<PathBuf, String> = Ok(self.config_dir.clone());
        stream! {
            let dir = match config_dir {
                Ok(d) => d,
                Err(e) => { yield RepoEvent::Error { code: Some("config_error".into()), error_class: None, message: e }; return; }
            };
            let dry = dry_run.as_ref().is_some_and(|v| to_bool(v, false));
            if org.is_empty() || name.is_empty() {
                yield validation_event("missing required parameter 'org' or 'name'");
                return;
            }
            let new_remote = match parse_remote(&remote) {
                Ok(r) => r,
                Err(e) => { yield validation_event(e); return; }
            };
            let loaded = match load_all(&dir) {
                Ok(l) => l,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(existing) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found"));
                return;
            };
            let Some(repo) = find_repo(existing, &name) else {
                yield not_found_event(format!("repo '{name}' not found under org '{org}'"));
                return;
            };
            if repo.remotes.iter().any(|r| r.url == new_remote.url) {
                yield validation_event(format!("duplicate remote url '{}'", new_remote.url));
                return;
            }
            let provider_map = loaded.global.provider_map.clone();
            if let Err(e) = derive_provider(&new_remote, &provider_map) {
                yield validation_event(e);
                return;
            }
            let mut updated = existing.clone();
            if let Some(repo_mut) = find_repo_mut(&mut updated, &name) {
                repo_mut.remotes.push(new_remote);
            }
            if !dry {
                let orgs_dir = dir.join("orgs");
                if let Err(e) = save_org(&orgs_dir, &updated) {
                    yield cfg_error_event(e);
                    return;
                }
            }
            let repo_after = find_repo(&updated, &name).unwrap();
            match repo_detail_event(org, name, repo_after, &provider_map) {
                Ok(ev) => yield ev,
                Err(msg) => yield validation_event(msg),
            }
        }
    }

    /// V5REPOS-8: drop a remote by URL.
    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        url = "Remote URL to drop",
        dry_run = "Preview without writing"
    ))]
    pub async fn remove_remote(
        &self,
        org: String,
        name: String,
        url: String,
        dry_run: Option<Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir: Result<PathBuf, String> = Ok(self.config_dir.clone());
        stream! {
            let dir = match config_dir {
                Ok(d) => d,
                Err(e) => { yield RepoEvent::Error { code: Some("config_error".into()), error_class: None, message: e }; return; }
            };
            let dry = dry_run.as_ref().is_some_and(|v| to_bool(v, false));
            if org.is_empty() || name.is_empty() || url.is_empty() {
                yield validation_event("missing required parameter 'org', 'name', or 'url'");
                return;
            }
            let loaded = match load_all(&dir) {
                Ok(l) => l,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(existing) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found"));
                return;
            };
            let Some(repo) = find_repo(existing, &name) else {
                yield not_found_event(format!("repo '{name}' not found under org '{org}'"));
                return;
            };
            if !repo.remotes.iter().any(|r| r.url.as_str() == url) {
                yield not_found_event(format!("remote url '{url}' not present on repo"));
                return;
            }
            if repo.remotes.len() == 1 {
                yield validation_event(format!(
                    "cannot remove last remote from repo '{name}'; use repos.remove to drop the entry"
                ));
                return;
            }
            let mut updated = existing.clone();
            if let Some(repo_mut) = find_repo_mut(&mut updated, &name) {
                if let Some(pos) = repo_mut.remotes.iter().position(|r| r.url.as_str() == url) {
                    repo_mut.remotes.remove(pos);
                }
            }
            if !dry {
                let orgs_dir = dir.join("orgs");
                if let Err(e) = save_org(&orgs_dir, &updated) {
                    yield cfg_error_event(e);
                    return;
                }
            }
            let provider_map = loaded.global.provider_map;
            let repo_after = find_repo(&updated, &name).unwrap();
            match repo_detail_event(org, name, repo_after, &provider_map) {
                Ok(ev) => yield ev,
                Err(msg) => yield validation_event(msg),
            }
        }
    }

    /// V5REPOS-13: read remote metadata, emit one `SyncDiff` per remote.
    /// HYPE-6: with `heal`, additionally resolve the repo's canonical owner
    /// on the forge and, on real divergence, repair it through the
    /// provider-scoped `migrate_one` (report + act); `dry_run` previews.
    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        remote = "Optional remote URL to limit scope",
        heal = "Repair owner divergence via migrate_one (HYPE-6)",
        path = "Optional local checkout dir to also flip on heal",
        dry_run = "With heal: preview only (default true); --dry-run false repairs"
    ))]
    pub async fn sync(
        &self,
        org: String,
        name: String,
        remote: Option<String>,
        heal: Option<Value>,
        path: Option<String>,
        dry_run: Option<Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir: Result<PathBuf, String> = Ok(self.config_dir.clone());
        stream! {
            let dir = match config_dir {
                Ok(d) => d,
                Err(e) => { yield RepoEvent::Error { code: Some("config_error".into()), error_class: None, message: e }; return; }
            };
            if org.is_empty() || name.is_empty() {
                yield validation_event("missing required parameter 'org' or 'name'");
                return;
            }
            let loaded = match load_all(&dir) {
                Ok(l) => l,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(org_cfg) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found"));
                return;
            };
            let Some(repo) = find_repo(org_cfg, &name) else {
                yield not_found_event(format!("repo '{name}' not found under org '{org}'"));
                return;
            };
            // Remote filter validation (ops::repo::sync_one handles
            // the filter internally; we only validate-for-error here).
            if let Some(filter_url) = remote.as_ref().filter(|s| !s.is_empty()) {
                if !repo.remotes.iter().any(|r| r.url.as_str() == filter_url) {
                    yield not_found_event(format!("remote url '{filter_url}' not present on repo"));
                    return;
                }
            }
            let resolver = YamlSecretStore::new(&dir);
            let repo_ref = RepoRef { org: OrgName::from(org.as_str()), name: RepoName::from(name.as_str()) };
            // V5LIFECYCLE-3: delegate to the single sync primitive.
            let outcomes = crate::v5::ops::repo::sync_one(
                repo,
                org_cfg,
                &loaded.global.provider_map,
                &resolver,
                remote.as_deref(),
            ).await;
            for o in outcomes {
                // Translate per-remote outcome into the RepoEvent::SyncDiff
                // wire shape (per-remote event for `repos.sync` per V5REPOS-13).
                yield RepoEvent::SyncDiff {
                    reference: (&repo_ref).into(),
                    url: o.remote.url.as_str().to_string(),
                    status: o.status.as_str().to_string(),
                    drift: o.drift.into_iter().map(|d| DriftField {
                        field: d.field,
                        local: d.local,
                        remote: d.remote,
                    }).collect(),
                    error_class: o.error_class.map(|e| e.as_str().to_string()),
                    remote: o.metadata,
                };
            }
            // HYPE-6: with `heal`, resolve canonical owner + repair on
            // divergence via the shared doctor/heal core (report + act).
            let do_heal = heal.as_ref().is_some_and(|v| to_bool(v, false));
            if do_heal {
                let dry = dry_run.as_ref().map_or(true, |v| to_bool(v, true));
                let dir_opt = path
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(std::path::PathBuf::from);
                let (events, rename) = doctor_one(
                    &dir, &org, org_cfg, repo, &loaded.global,
                    &resolver, true, dir_opt.as_deref(), dry,
                ).await;
                for e in events { yield e; }
                if let Some(r) = rename {
                    if !dry {
                        let _ = write_rename_map(&dir, std::slice::from_ref(&r));
                    }
                }
            }
        }
    }

    /// V5REPOS-14: sequential per-remote metadata write per D4.
    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        remote = "Optional single-remote scope",
        fields = "Optional JSON field override; defaults to local metadata",
        dry_run = "Preview without forge writes"
    ))]
    pub async fn push(
        &self,
        org: String,
        name: String,
        remote: Option<String>,
        fields: Option<Value>,
        dry_run: Option<Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir: Result<PathBuf, String> = Ok(self.config_dir.clone());
        stream! {
            let dir = match config_dir {
                Ok(d) => d,
                Err(e) => { yield RepoEvent::Error { code: Some("config_error".into()), error_class: None, message: e }; return; }
            };
            let dry = dry_run.as_ref().is_some_and(|v| to_bool(v, false));
            if org.is_empty() || name.is_empty() {
                yield validation_event("missing required parameter 'org' or 'name'");
                return;
            }
            let loaded = match load_all(&dir) {
                Ok(l) => l,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(org_cfg) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found"));
                return;
            };
            let Some(repo) = find_repo(org_cfg, &name) else {
                yield not_found_event(format!("repo '{name}' not found under org '{org}'"));
                return;
            };
            let to_apply = match fields {
                Some(v) => match parse_fields(&v) {
                    Ok(m) => m,
                    Err(e) => { yield validation_event(e); return; }
                },
                None => metadata_from_local(&repo.metadata),
            };
            if to_apply.is_empty() {
                yield validation_event("no fields to push; supply `fields` or declare repo.metadata locally");
                return;
            }
            // V5PARITY-34: scope by per-repo forges, then narrow by URL filter.
            let scoped: Vec<&Remote> = crate::v5::ops::repo::filter_remotes_by_forges(
                repo, &loaded.global.provider_map,
            );
            if crate::v5::ops::repo::all_remotes_excluded(repo, &loaded.global.provider_map) {
                yield RepoEvent::Error {
                    code: Some("forge_excluded".into()),
                    error_class: None,
                    message: format!("repo '{name}' has remotes but per-repo `forges` scope excludes all of them"),
                };
                return;
            }
            let remotes: Vec<&Remote> = if let Some(filter_url) = remote.as_ref().filter(|s| !s.is_empty()) {
                let matched: Vec<&Remote> = scoped.into_iter().filter(|r| r.url.as_str() == filter_url).collect();
                if matched.is_empty() {
                    yield not_found_event(format!("remote url '{filter_url}' not present on repo (or excluded by `forges` scope)"));
                    return;
                }
                matched
            } else {
                scoped
            };
            let resolver = YamlSecretStore::new(&dir);
            let token_ref = org_cfg
                .forge
                .credentials
                .iter()
                .find(|c| matches!(c.cred_type, CredentialType::Token))
                .map(|c| c.key.clone());
            let fallback_token_ref = Some(crate::v5::ops::repo::default_token_ref_for(org_cfg));
            let repo_ref = RepoRef { org: OrgName::from(org.as_str()), name: RepoName::from(name.as_str()) };

            let mut succeeded: Vec<String> = Vec::new();
            let mut errored: Vec<PushErrored> = Vec::new();
            let mut aborted = false;

            for r in remotes {
                let url_s = r.url.as_str().to_string();
                let provider = match derive_provider(r, &loaded.global.provider_map) {
                    Ok(p) => p,
                    Err(e) => {
                        let ev = PushErrored {
                            url: url_s.clone(),
                            error_class: "network".into(),
                            message: e.clone(),
                        };
                        yield RepoEvent::PushRemoteError {
                            url: url_s.clone(),
                            error_class: ev.error_class.clone(),
                            message: ev.message.clone(),
                        };
                        errored.push(ev);
                        aborted = true;
                        break;
                    }
                };
                if dry {
                    let names: Vec<String> = to_apply
                        .keys()
                        .map(|k| k.as_str().to_string())
                        .collect();
                    yield RepoEvent::PushRemoteOk { url: url_s.clone(), fields: names };
                    succeeded.push(url_s);
                    continue;
                }
                // V5LIFECYCLE-4: write via ops::repo helper.
                let _ = provider; // logging placeholder; provider is re-derived inside the helper
                match crate::v5::ops::repo::write_metadata_on_forge(
                    r, &repo_ref, &to_apply, &loaded.global.provider_map, &resolver, token_ref.as_deref(), fallback_token_ref.clone(),
                ).await {
                    Ok(applied) => {
                        let names: Vec<String> = applied
                            .keys()
                            .map(|k| k.as_str().to_string())
                            .collect();
                        yield RepoEvent::PushRemoteOk { url: url_s.clone(), fields: names };
                        succeeded.push(url_s);
                    }
                    Err(e) => {
                        let class = e.class.as_str().to_string();
                        let message = e.message.clone();
                        yield RepoEvent::PushRemoteError {
                            url: url_s.clone(),
                            error_class: class.clone(),
                            message: message.clone(),
                        };
                        errored.push(PushErrored { url: url_s, error_class: class, message });
                        aborted = true;
                        break;
                    }
                }
            }
            yield RepoEvent::PushSummary { succeeded, errored, aborted };
        }
    }

    // ==================================================================
    // V5LIFECYCLE-6: repos.delete — soft (privatize + mark dismissed).
    // ==================================================================

    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        dry_run = "Preview without writing"
    ))]
    pub async fn delete(
        &self,
        org: String,
        name: String,
        dry_run: Option<Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let dry = dry_run.as_ref().is_some_and(|v| to_bool(v, false));
            if org.is_empty() || name.is_empty() {
                yield validation_event("missing required parameter 'org' or 'name'");
                return;
            }
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(existing) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found")); return;
            };
            let Some(repo) = crate::v5::ops::state::find_repo(existing, &name) else {
                yield not_found_event(format!("repo '{name}' not found under org '{org}'")); return;
            };
            let repo_ref = RepoRef { org: OrgName::from(org.as_str()), name: RepoName::from(name.as_str()) };
            let wire = RepoRefWire::from(&repo_ref);
            // Protection guard.
            if repo.metadata.as_ref().is_some_and(|m| m.protected) {
                yield RepoEvent::Error {
                    code: Some("protected".into()),
                    error_class: None,
                    message: format!("repo '{name}' is protected; toggle via repos.protect first"),
                };
                return;
            }
            // Already-dismissed idempotency.
            let already = repo.metadata.as_ref().map_or(crate::v5::config::RepoLifecycle::Active, |m| m.lifecycle)
                == crate::v5::config::RepoLifecycle::Dismissed;
            if already {
                let prev: Vec<String> = repo.metadata.as_ref()
                    .map(|m| m.privatized_on.iter().map(|p| match p {
                        ProviderKind::Github => "github".to_string(),
                        ProviderKind::Codeberg => "codeberg".to_string(),
                        ProviderKind::Gitlab => "gitlab".to_string(),
                    }).collect())
                    .unwrap_or_default();
                yield RepoEvent::RepoDismissed { reference: wire, privatized_on: prev, already: true };
                return;
            }
            // Privatize on every remote.
            let resolver = YamlSecretStore::new(&config_dir);
            let token_ref = crate::v5::ops::repo::token_ref_for(existing);
            let fallback_token_ref = Some(crate::v5::ops::repo::default_token_ref_for(existing));
            let mut privatized: std::collections::BTreeSet<ProviderKind> = std::collections::BTreeSet::new();
            // V5PARITY-34: only privatize forges in scope.
            let scoped = crate::v5::ops::repo::filter_remotes_by_forges(repo, &loaded.global.provider_map);
            for r in scoped {
                let provider = match crate::v5::ops::repo::derive_provider(r, &loaded.global.provider_map) {
                    Ok(p) => p,
                    Err(e) => { yield validation_event(e); continue; }
                };
                let provider_s = match provider {
                    ProviderKind::Github => "github".to_string(),
                    ProviderKind::Codeberg => "codeberg".to_string(),
                    ProviderKind::Gitlab => "gitlab".to_string(),
                };
                let url_s = r.url.as_str().to_string();
                if dry {
                    yield RepoEvent::ForgePrivatized { reference: wire.clone(), provider: provider_s.clone(), url: url_s, dry_run: true };
                    privatized.insert(provider);
                    continue;
                }
                match crate::v5::ops::repo::privatize_on_forge(r, &repo_ref, &loaded.global.provider_map, &resolver, token_ref, fallback_token_ref.clone()).await {
                    Ok(()) => {
                        privatized.insert(provider);
                        yield RepoEvent::ForgePrivatized { reference: wire.clone(), provider: provider_s, url: url_s, dry_run: false };
                    }
                    Err(e) => {
                        yield RepoEvent::PrivatizeError {
                            reference: wire.clone(),
                            provider: provider_s,
                            error_class: e.class.as_str().to_string(),
                            message: e.message,
                        };
                    }
                }
            }
            let priv_list: Vec<String> = privatized.iter().map(|p| match p {
                ProviderKind::Github => "github".to_string(),
                ProviderKind::Codeberg => "codeberg".to_string(),
                ProviderKind::Gitlab => "gitlab".to_string(),
            }).collect();
            if !dry {
                let mut updated = existing.clone();
                if let Some(mr) = crate::v5::ops::state::find_repo_mut(&mut updated, &name) {
                    crate::v5::ops::repo::dismiss(mr, privatized);
                }
                let orgs_dir = config_dir.join("orgs");
                if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &updated) {
                    yield cfg_error_event(e); return;
                }
            }
            yield RepoEvent::RepoDismissed { reference: wire, privatized_on: priv_list, already: false };
        }
    }

    // ==================================================================
    // V5LIFECYCLE-7: repos.purge — hard-delete, gated on dismissed.
    // ==================================================================

    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        dry_run = "Preview without writing"
    ))]
    pub async fn purge(
        &self,
        org: String,
        name: String,
        dry_run: Option<Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let dry = dry_run.as_ref().is_some_and(|v| to_bool(v, false));
            if org.is_empty() || name.is_empty() {
                yield validation_event("missing required parameter 'org' or 'name'");
                return;
            }
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(existing) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found")); return;
            };
            let Some(repo) = crate::v5::ops::state::find_repo(existing, &name) else {
                yield not_found_event(format!("repo '{name}' not found under org '{org}'")); return;
            };
            let repo_ref = RepoRef { org: OrgName::from(org.as_str()), name: RepoName::from(name.as_str()) };
            let wire = RepoRefWire::from(&repo_ref);
            if repo.metadata.as_ref().is_some_and(|m| m.protected) {
                yield RepoEvent::Error {
                    code: Some("protected".into()),
                    error_class: None,
                    message: format!("repo '{name}' is protected"),
                };
                return;
            }
            if repo.metadata.as_ref().map_or(crate::v5::config::RepoLifecycle::Active, |m| m.lifecycle) != crate::v5::config::RepoLifecycle::Dismissed {
                yield RepoEvent::Error {
                    code: Some("not_dismissed".into()),
                    error_class: None,
                    message: "purge requires lifecycle: dismissed; run repos.delete first".into(),
                };
                return;
            }
            // Forge-delete every remote.
            // V5PARITY-34: only forges in scope; purge respects per-repo policy.
            let resolver = YamlSecretStore::new(&config_dir);
            let token_ref = crate::v5::ops::repo::token_ref_for(existing);
            let fallback_token_ref = Some(crate::v5::ops::repo::default_token_ref_for(existing));
            let scoped_remotes = crate::v5::ops::repo::filter_remotes_by_forges(repo, &loaded.global.provider_map);
            for r in scoped_remotes {
                let provider = match crate::v5::ops::repo::derive_provider(r, &loaded.global.provider_map) {
                    Ok(p) => p,
                    Err(e) => { yield validation_event(e); continue; }
                };
                let provider_s = match provider {
                    ProviderKind::Github => "github".to_string(),
                    ProviderKind::Codeberg => "codeberg".to_string(),
                    ProviderKind::Gitlab => "gitlab".to_string(),
                };
                let url_s = r.url.as_str().to_string();
                if dry {
                    yield RepoEvent::ForgeDeleted { reference: wire.clone(), provider: provider_s, url: url_s, note: Some("dry_run".into()) };
                    continue;
                }
                match crate::v5::ops::repo::delete_on_forge(r, &repo_ref, &loaded.global.provider_map, &resolver, token_ref, fallback_token_ref.clone()).await {
                    Ok(()) => yield RepoEvent::ForgeDeleted { reference: wire.clone(), provider: provider_s, url: url_s, note: None },
                    Err(e) if matches!(e.class, crate::v5::adapters::ForgeErrorClass::NotFound) => {
                        yield RepoEvent::ForgeDeleted { reference: wire.clone(), provider: provider_s, url: url_s, note: Some("already gone".into()) };
                    }
                    Err(e) => {
                        yield RepoEvent::PurgeError {
                            reference: wire.clone(),
                            provider: provider_s,
                            error_class: e.class.as_str().to_string(),
                            message: e.message,
                        };
                    }
                }
            }
            if !dry {
                let mut updated = existing.clone();
                let _ = crate::v5::ops::repo::purge(&mut updated, &RepoName::from(name.as_str()));
                let orgs_dir = config_dir.join("orgs");
                if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &updated) {
                    yield cfg_error_event(e); return;
                }
            }
            yield RepoEvent::RepoPurged { reference: wire };
        }
    }

    // ==================================================================
    // V5LIFECYCLE-8: repos.protect — toggle protection bit.
    // ==================================================================

    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        protected = "Target state",
        dry_run = "Preview without writing"
    ))]
    pub async fn protect(
        &self,
        org: String,
        name: String,
        protected: Option<Value>,
        dry_run: Option<Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let dry = dry_run.as_ref().is_some_and(|v| to_bool(v, false));
            let target = protected.as_ref().is_some_and(|v| to_bool(v, false));
            if org.is_empty() || name.is_empty() {
                yield validation_event("missing required parameter 'org' or 'name'");
                return;
            }
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(existing) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found")); return;
            };
            if crate::v5::ops::state::find_repo(existing, &name).is_none() {
                yield not_found_event(format!("repo '{name}' not found under org '{org}'")); return;
            }
            let repo_ref = RepoRef { org: OrgName::from(org.as_str()), name: RepoName::from(name.as_str()) };
            let wire = RepoRefWire::from(&repo_ref);
            if !dry {
                let mut updated = existing.clone();
                if let Some(mr) = crate::v5::ops::state::find_repo_mut(&mut updated, &name) {
                    let md = mr.metadata.get_or_insert_with(RepoMetadataLocal::default);
                    md.protected = target;
                }
                let orgs_dir = config_dir.join("orgs");
                if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &updated) {
                    yield cfg_error_event(e); return;
                }
            }
            yield RepoEvent::RepoProtectionSet { reference: wire, protected: target };
        }
    }

    // ==================================================================
    // V5LIFECYCLE-9: repos.init — write .hyperforge/config.toml.
    // ==================================================================

    #[plexus_macros::method(params(
        target_path = "Repo checkout directory (note: named target_path to avoid synapse's path-autoexpansion)",
        org = "Owning org",
        repo_name = "Repo identifier",
        forges = "JSON array of provider names",
        default_branch = "Default branch (defaults to main)",
        visibility = "private|public|internal (default private)",
        description = "Free-text description",
        force = "Overwrite existing .hyperforge/config.toml",
        dry_run = "Preview without writing"
    ))]
    pub async fn init(
        &self,
        target_path: String,
        org: String,
        repo_name: String,
        forges: Option<Value>,
        default_branch: Option<String>,
        visibility: Option<String>,
        description: Option<String>,
        force: Option<Value>,
        dry_run: Option<Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        stream! {
            let dry = dry_run.as_ref().is_some_and(|v| to_bool(v, false));
            let force_b = force.as_ref().is_some_and(|v| to_bool(v, false));
            if target_path.is_empty() || org.is_empty() || repo_name.is_empty() {
                yield validation_event("missing required parameter 'target_path', 'org', or 'repo_name'");
                return;
            }
            let forges_list: Vec<ProviderKind> = match forges.as_ref() {
                None => vec![ProviderKind::Github],
                Some(v) => {
                    let arr = if let Some(s) = v.as_str() {
                        serde_json::from_str::<Vec<String>>(s).unwrap_or_default()
                    } else if let Some(a) = v.as_array() {
                        a.iter().filter_map(|e| e.as_str().map(String::from)).collect()
                    } else { vec![] };
                    arr.into_iter().filter_map(|s| match s.as_str() {
                        "github" => Some(ProviderKind::Github),
                        "codeberg" => Some(ProviderKind::Codeberg),
                        "gitlab" => Some(ProviderKind::Gitlab),
                        _ => None,
                    }).collect()
                }
            };
            let cfg = crate::v5::ops::fs::HyperforgeRepoConfig {
                repo_name: repo_name.clone(),
                org: OrgName::from(org.as_str()),
                forges: forges_list,
                default_branch: default_branch.or_else(|| Some("main".into())),
                visibility,
                description,
            };
            let path = std::path::PathBuf::from(&target_path);
            if dry {
                yield RepoEvent::HyperforgeConfigWritten {
                    path: path.join(".hyperforge").join("config.toml").display().to_string(),
                    repo_name: repo_name.clone(),
                    org: org.clone(),
                };
                return;
            }
            match crate::v5::ops::fs::write_hyperforge_config(&path, &cfg, force_b) {
                Ok(written_path) => {
                    yield RepoEvent::HyperforgeConfigWritten {
                        path: written_path.display().to_string(),
                        repo_name,
                        org,
                    };
                }
                Err(e) => {
                    yield RepoEvent::Error {
                        code: Some(e.code().into()),
                        error_class: None,
                        message: e.to_string(),
                    };
                }
            }
        }
    }

    // ==================================================================
    // V5PARITY-2: repos.import — walk a forge and register missing repos.
    // ==================================================================

    #[plexus_macros::method(params(
        org = "Org name",
        forge = "Optional provider filter (github|codeberg|gitlab); default = org's declared forge",
        dry_run = "Preview without writing"
    ))]
    pub async fn import(
        &self,
        org: String,
        forge: Option<String>,
        dry_run: Option<Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let dry = dry_run.as_ref().is_some_and(|v| to_bool(v, false));
            if org.is_empty() {
                yield validation_event("missing required parameter 'org'");
                return;
            }
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(org_cfg) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found"));
                return;
            };
            // Pick the provider: explicit --forge wins; otherwise the org's declared forge.provider.
            let provider = if let Some(f) = forge.as_deref().filter(|s| !s.is_empty()) {
                match f {
                    "github" => ProviderKind::Github,
                    "codeberg" => ProviderKind::Codeberg,
                    "gitlab" => ProviderKind::Gitlab,
                    other => { yield validation_event(format!("unknown provider: {other}")); return; }
                }
            } else {
                org_cfg.forge.provider
            };
            let resolver = YamlSecretStore::new(&config_dir);
            let token_ref = crate::v5::ops::repo::token_ref_for(org_cfg);
            let fallback_token_ref = Some(crate::v5::ops::repo::default_token_ref_for(org_cfg));
            let remote_repos = match crate::v5::ops::repo::list_on_forge(
                provider, &OrgName::from(org.as_str()), &resolver, token_ref, fallback_token_ref.clone(),
            ).await {
                Ok(v) => v,
                Err(e) => {
                    yield RepoEvent::Error {
                        code: Some(e.class.as_str().into()),
                        error_class: Some(e.class.as_str().into()),
                        message: e.message,
                    };
                    return;
                }
            };
            let total = u32::try_from(remote_repos.len()).unwrap_or(u32::MAX);
            let mut added: u32 = 0;
            let mut skipped: u32 = 0;
            // Clone the org for mutation.
            let mut updated = org_cfg.clone();
            for rr in &remote_repos {
                // Skip if already registered.
                let already = updated.repos.iter().any(|r| r.name.as_str() == rr.name);
                if already { skipped += 1; continue; }
                // Append.
                let new_repo = crate::v5::config::OrgRepo {
                    name: RepoName::from(rr.name.as_str()),
                    remotes: vec![crate::v5::config::Remote {
                        url: crate::v5::config::RemoteUrl::from(rr.url.as_str()),
                        provider: None,
                    }],
                    forges: None,
                    metadata: None,
                };
                updated.repos.push(new_repo);
                added += 1;
                yield RepoEvent::RepoImported {
                    reference: RepoRefWire {
                        org: org.clone(),
                        name: rr.name.clone(),
                    },
                    url: rr.url.clone(),
                };
            }
            if !dry && added > 0 {
                let orgs_dir = config_dir.join("orgs");
                if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &updated) {
                    yield cfg_error_event(e); return;
                }
            }
            yield RepoEvent::ImportSummary {
                org: org.clone(),
                total,
                added,
                skipped,
            };
        }
    }

    // ==================================================================
    // V5PARITY-3: git transport methods.
    // ==================================================================

    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        dest = "Destination directory (must not exist)"
    ))]
    pub async fn clone(
        &self,
        org: String,
        name: String,
        dest: String,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            if org.is_empty() || name.is_empty() || dest.is_empty() {
                yield validation_event("missing required parameter 'org', 'name', or 'dest'");
                return;
            }
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(org_cfg) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found")); return;
            };
            let Some(repo) = crate::v5::ops::state::find_repo(org_cfg, &name) else {
                yield not_found_event(format!("repo '{name}' not found under org '{org}'")); return;
            };
            // V5PARITY-34: clone uses the canonical remote in scope.
            let Some(first) = crate::v5::ops::repo::canonical_remote_in_scope(
                repo, &loaded.global.provider_map,
            ) else {
                if crate::v5::ops::repo::all_remotes_excluded(repo, &loaded.global.provider_map) {
                    yield RepoEvent::Error {
                        code: Some("forge_excluded".into()), error_class: None,
                        message: format!("repo '{name}' has remotes but `forges` scope excludes all"),
                    };
                } else {
                    yield validation_event(format!("repo '{name}' has no remotes"));
                }
                return;
            };
            let dest_path = std::path::PathBuf::from(&dest);
            let url = first.url.as_str();
            // V5PARITY-5: per-org SSH key routing. If the org has a
            // `ssh_key` credential, forward it as GIT_SSH_COMMAND for
            // the clone subprocess, then persist it via `core.sshCommand`
            // in the newly-cloned repo so later fetch/pull/push reuse it.
            let key_path = ssh_key_for_org(org_cfg);
            let ssh_cmd = key_path.as_ref().map(|p| crate::v5::ops::git::format_ssh_command(p));
            let env: Vec<(&str, &str)> = match ssh_cmd.as_deref() {
                Some(s) => vec![("GIT_SSH_COMMAND", s)],
                None => Vec::new(),
            };
            let clone_result = if env.is_empty() {
                crate::v5::ops::git::clone_repo(url, &dest_path)
            } else {
                crate::v5::ops::git::clone_repo_with_env(url, &dest_path, &env)
            };
            match clone_result {
                Ok(()) => {
                    if let Some(p) = key_path.as_ref() {
                        let _ = crate::v5::ops::git::set_ssh_command(&dest_path, p);
                    }
                    yield RepoEvent::CloneDone {
                        reference: RepoRefWire { org: org.clone(), name: name.clone() },
                        url: url.to_string(),
                        dest,
                    };
                }
                Err(e) => yield RepoEvent::Error {
                    code: Some(e.code().into()),
                    error_class: None,
                    message: e.to_string(),
                },
            }
        }
    }

    #[plexus_macros::method(params(
        path = "Repo checkout directory",
        remote = "Optional remote name (default: all remotes)"
    ))]
    pub async fn fetch(
        &self,
        path: String,
        remote: Option<String>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        stream! {
            if path.is_empty() {
                yield validation_event("missing required parameter 'path'"); return;
            }
            let dir = std::path::PathBuf::from(&path);
            match crate::v5::ops::git::fetch(&dir, remote.as_deref()) {
                Ok(()) => yield RepoEvent::FetchDone {
                    reference: RepoRefWire { org: String::new(), name: String::new() },
                    remote,
                },
                Err(e) => yield RepoEvent::Error {
                    code: Some(e.code().into()),
                    error_class: None,
                    message: e.to_string(),
                },
            }
        }
    }

    #[plexus_macros::method(params(
        path = "Repo checkout directory",
        remote = "Remote name (default: origin)",
        branch = "Branch (default: current)"
    ))]
    pub async fn pull(
        &self,
        path: String,
        remote: Option<String>,
        branch: Option<String>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        stream! {
            if path.is_empty() {
                yield validation_event("missing required parameter 'path'"); return;
            }
            let dir = std::path::PathBuf::from(&path);
            let r = remote.unwrap_or_else(|| "origin".into());
            let b = match branch {
                Some(v) => v,
                None => match crate::v5::ops::git::status(&dir) {
                    Ok(s) => s.branch.unwrap_or_else(|| "main".into()),
                    Err(e) => { yield RepoEvent::Error {
                        code: Some(e.code().into()), error_class: None, message: e.to_string(),
                    }; return; }
                },
            };
            match crate::v5::ops::git::pull_ff(&dir, &r, &b) {
                Ok(()) => yield RepoEvent::PullDone {
                    reference: RepoRefWire { org: String::new(), name: String::new() },
                    remote: r,
                    branch: b,
                },
                Err(e) => yield RepoEvent::Error {
                    code: Some(e.code().into()),
                    error_class: None,
                    message: e.to_string(),
                },
            }
        }
    }

    #[plexus_macros::method(params(
        path = "Repo checkout directory",
        remote = "Remote name (default: origin)",
        branch = "Branch (default: current)"
    ))]
    pub async fn push_refs(
        &self,
        path: String,
        remote: Option<String>,
        branch: Option<String>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        stream! {
            if path.is_empty() {
                yield validation_event("missing required parameter 'path'"); return;
            }
            let dir = std::path::PathBuf::from(&path);
            let r = remote.unwrap_or_else(|| "origin".into());
            match crate::v5::ops::git::push_refs(&dir, &r, branch.as_deref()) {
                Ok(()) => yield RepoEvent::PushRefsDone {
                    reference: RepoRefWire { org: String::new(), name: String::new() },
                    remote: r,
                    branch,
                },
                Err(e) => yield RepoEvent::Error {
                    code: Some(e.code().into()),
                    error_class: None,
                    message: e.to_string(),
                },
            }
        }
    }

    /// HYPERFORGE-60: push branches and tags to every forge a repo is
    /// registered on (its org-yaml remotes), each with that forge's own
    /// credential. Dry run unless `execute`; never forced.
    #[plexus_macros::method(params(
        workspace = "Workspace whose members to mirror (default: the configured default workspace)",
        org = "Org: with `name`, one repo; otherwise narrows the workspace",
        name = "Repo name: one repo, or narrows the workspace",
        path = "Checkout of one repo (needs `org` and `name`)",
        branches = "Branches to push (default: the checkout's current branch)",
        all_branches = "Push every local branch (default false)",
        tags = "Push every tag (default true)",
        status = "Compare each forge with the checkout; push nothing (default false)",
        execute = "Push (default false: plan only)"
    ))]
    #[allow(clippy::too_many_arguments)]
    pub async fn mirror(
        &self,
        workspace: Option<String>,
        org: Option<String>,
        name: Option<String>,
        path: Option<String>,
        branches: Option<Vec<String>>,
        all_branches: Option<bool>,
        tags: Option<bool>,
        status: Option<bool>,
        execute: Option<bool>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        use crate::v5::ops::mirror as m;
        let config_dir = self.config_dir.clone();
        stream! {
            let status_mode = status.unwrap_or(false);
            let do_execute = execute.unwrap_or(false);
            if status_mode && do_execute {
                yield validation_event("`status` and `execute` are exclusive");
                return;
            }
            let all_b = all_branches.unwrap_or(false);
            let with_tags = tags.unwrap_or(true);
            let loaded = match load_all(&config_dir) {
                Ok(l) => l,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let resolver = YamlSecretStore::new(&config_dir);
            let trim = |s: &Option<String>| s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
            let (org_f, name_f, path_f) = (trim(&org), trim(&name), trim(&path));

            // (org, name, checkout dir)
            let mut members: Vec<(String, String, PathBuf)> = Vec::new();
            if let Some(p) = path_f {
                let (Some(o), Some(n)) = (org_f.clone(), name_f.clone()) else {
                    yield validation_event("`path` needs `org` and `name`");
                    return;
                };
                members.push((o, n, PathBuf::from(p)));
            } else {
                let ws_name = trim(&workspace)
                    .map(|w| crate::v5::config::WorkspaceName::from(w.as_str()))
                    .or_else(|| loaded.global.default_workspace.clone());
                let Some(ws_name) = ws_name else {
                    yield validation_event("no `workspace` given and no default_workspace configured");
                    return;
                };
                let Some(ws) = loaded.workspaces.get(&ws_name) else {
                    yield not_found_event(format!("workspace '{}' not found", ws_name.as_str()));
                    return;
                };
                let root = PathBuf::from(ws.path.as_str());
                for entry in &ws.repos {
                    let (o, n, dir) = match entry {
                        crate::v5::config::WorkspaceRepo::Shorthand(s) => match s.split_once('/') {
                            Some((o, n)) => (o.to_string(), n.to_string(), n.to_string()),
                            None => continue,
                        },
                        crate::v5::config::WorkspaceRepo::Object { reference, dir } => (
                            reference.org.as_str().to_string(),
                            reference.name.as_str().to_string(),
                            dir.clone(),
                        ),
                    };
                    if org_f.as_deref().is_some_and(|f| f != o) || name_f.as_deref().is_some_and(|f| f != n) {
                        continue;
                    }
                    members.push((o, n, root.join(dir)));
                }
                if members.is_empty() {
                    yield validation_event("no workspace member matched `org`/`name`");
                    return;
                }
            }

            let (mut repos_n, mut remotes_n, mut planned, mut pushed, mut failed, mut skipped) = (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
            let mut failures: Vec<String> = Vec::new();
            for (o, n, dir) in members {
                repos_n += 1;
                let reference = RepoRefWire { org: o.clone(), name: n.clone() };
                let Some(org_cfg) = loaded.orgs.get(&OrgName::from(o.as_str())) else {
                    skipped += 1;
                    yield RepoEvent::MirrorSkipped { reference, reason: format!("org '{o}' is not configured") };
                    continue;
                };
                let Some(repo) = find_repo(org_cfg, &n) else {
                    skipped += 1;
                    yield RepoEvent::MirrorSkipped { reference, reason: format!("'{o}/{n}' is not registered on its org") };
                    continue;
                };
                if !dir.join(".git").exists() {
                    skipped += 1;
                    yield RepoEvent::MirrorSkipped { reference, reason: format!("no checkout at {}", dir.display()) };
                    continue;
                }
                let (targets, unknown) = m::targets(repo, &loaded.global.provider_map);
                for u in unknown {
                    yield RepoEvent::MirrorSkipped { reference: reference.clone(), reason: u };
                }
                if targets.is_empty() {
                    skipped += 1;
                    yield RepoEvent::MirrorSkipped { reference, reason: "no registered remote in its forge scope".into() };
                    continue;
                }
                let names: Vec<String> = match branches.as_ref().filter(|b| !b.is_empty()) {
                    Some(b) => b.clone(),
                    None if all_b => Vec::new(),
                    None => match crate::v5::ops::git::status(&dir).ok().and_then(|s| s.branch) {
                        Some(b) => vec![b],
                        None if with_tags => Vec::new(),
                        None => {
                            skipped += 1;
                            yield RepoEvent::MirrorSkipped { reference, reason: "detached HEAD and no `branches` given".into() };
                            continue;
                        }
                    },
                };
                let specs = if status_mode {
                    Vec::new()
                } else {
                    match m::refspecs(&dir, &names, all_b, with_tags) {
                        Ok(s) => s,
                        Err(e) => {
                            skipped += 1;
                            yield RepoEvent::MirrorSkipped { reference, reason: e };
                            continue;
                        }
                    }
                };
                for t in targets {
                    remotes_n += 1;
                    let provider = m::provider_label(t.provider).to_string();
                    let cred = if m::is_http(&t.url) {
                        let refs = m::credential_refs(org_cfg, t.provider, m::url_owner(&t.url).as_deref());
                        m::resolve(&resolver, &refs)
                    } else {
                        None
                    };
                    if status_mode {
                        match m::compare(&dir, &t, &names, all_b, cred.as_ref()) {
                            Ok(c) => yield RepoEvent::MirrorStatus {
                                reference: reference.clone(),
                                url: t.url.clone(),
                                provider,
                                branches: c.branches.iter().map(|(b, s)| format!("{b}: {s}")).collect(),
                                tags_missing: c.tags_missing,
                                tags_differ: c.tags_differ,
                            },
                            Err(e) => {
                                failed += 1;
                                failures.push(format!("{o}/{n} → {}", t.url));
                                yield RepoEvent::MirrorFailed { reference: reference.clone(), url: t.url.clone(), provider, message: e };
                            }
                        }
                    } else if do_execute {
                        match m::push(&dir, &t, &specs, cred.as_ref()) {
                            Ok(p) => {
                                if p.rejected.is_empty() { pushed += 1; } else {
                                    failed += 1;
                                    failures.push(format!("{o}/{n} → {} ({} refs rejected)", t.url, p.rejected.len()));
                                }
                                yield RepoEvent::MirrorPushed {
                                    reference: reference.clone(),
                                    url: t.url.clone(),
                                    provider,
                                    updated: p.updated,
                                    up_to_date: p.up_to_date,
                                    rejected: p.rejected,
                                };
                            }
                            Err(e) => {
                                failed += 1;
                                failures.push(format!("{o}/{n} → {}", t.url));
                                yield RepoEvent::MirrorFailed { reference: reference.clone(), url: t.url.clone(), provider, message: e };
                            }
                        }
                    } else {
                        planned += 1;
                        let auth = match (&cred, m::is_http(&t.url)) {
                            (Some(c), _) => c.reference.clone(),
                            (None, true) => "git credential helper (no token in the secret store)".into(),
                            (None, false) if t.url.contains("://") && !t.url.starts_with("file://") => "ssh".into(),
                            (None, false) if t.url.contains('@') => "ssh".into(),
                            (None, false) => "local".into(),
                        };
                        yield RepoEvent::MirrorPlan { reference: reference.clone(), url: t.url.clone(), provider, refspecs: specs.clone(), auth };
                    }
                }
            }
            yield RepoEvent::MirrorSummary {
                repos: repos_n,
                remotes: remotes_n,
                planned,
                pushed,
                failed,
                skipped,
                dry_run: !status_mode && !do_execute,
            };
            if !failures.is_empty() {
                yield RepoEvent::Error {
                    code: Some("partial".into()),
                    error_class: None,
                    message: format!("{} of {} forges failed: {}", failures.len(), remotes_n, failures.join("; ")),
                };
            }
        }
    }

    #[plexus_macros::method(params(
        path = "Repo checkout directory, on the branch to propose",
        title = "Pull request title",
        body = "Pull request body",
        base = "Branch to merge into (default the repo's default branch)",
        draft = "Open as a draft (default false)",
        dry_run = "Check everything; open nothing (default false)"
    ))]
    #[allow(clippy::too_many_arguments)]
    pub async fn pr(
        &self,
        path: String,
        title: String,
        body: Option<String>,
        base: Option<String>,
        draft: Option<bool>,
        dry_run: Option<bool>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let args = crate::v5::ops::pr::Args {
                dir: std::path::PathBuf::from(path),
                title,
                body: body.unwrap_or_default(),
                base,
                draft: draft.unwrap_or(false),
                dry_run: dry_run.unwrap_or(false),
            };
            match crate::v5::ops::pr::open(&config_dir, args).await {
                Ok(p) => yield RepoEvent::PullRequest {
                    repo: p.repo, head: p.head, base: p.base, title: p.title,
                    dry_run: p.dry_run, number: p.number, url: p.url,
                },
                Err((code, message)) => yield RepoEvent::Error {
                    code: Some(code.into()), error_class: None, message,
                },
            }
        }
    }

    #[plexus_macros::method(params(path = "Repo checkout directory"))]
    pub async fn status(
        &self,
        path: String,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        stream! {
            if path.is_empty() {
                yield validation_event("missing required parameter 'path'"); return;
            }
            let dir = std::path::PathBuf::from(&path);
            match crate::v5::ops::git::status(&dir) {
                Ok(s) => {
                    let dirty = s.dirty();
                    yield RepoEvent::RepoStatus {
                        path,
                        branch: s.branch,
                        upstream: s.upstream,
                        ahead: s.ahead,
                        behind: s.behind,
                        staged: s.staged,
                        unstaged: s.unstaged,
                        untracked: s.untracked,
                        dirty,
                    }
                }
                Err(e) => yield RepoEvent::Error {
                    code: Some(e.code().into()), error_class: None, message: e.to_string(),
                },
            }
        }
    }

    #[plexus_macros::method(params(path = "Repo checkout directory"))]
    pub async fn dirty(
        &self,
        path: String,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        stream! {
            if path.is_empty() {
                yield validation_event("missing required parameter 'path'"); return;
            }
            let dir = std::path::PathBuf::from(&path);
            match crate::v5::ops::git::is_dirty(&dir) {
                Ok(d) => yield RepoEvent::RepoDirty { path, dirty: d },
                Err(e) => yield RepoEvent::Error {
                    code: Some(e.code().into()), error_class: None, message: e.to_string(),
                },
            }
        }
    }

    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        transport = "ssh | https",
        path = "Optional checkout directory; if given, .git/config is updated too"
    ))]
    pub async fn set_transport(
        &self,
        org: String,
        name: String,
        transport: String,
        path: Option<String>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            if org.is_empty() || name.is_empty() || transport.is_empty() {
                yield validation_event("missing required parameter 'org', 'name', or 'transport'"); return;
            }
            match transport.as_str() {
                "ssh" | "https" => {}
                other => { yield validation_event(format!("unknown transport: {other}")); return; }
            }
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(existing) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found")); return;
            };
            let Some(_) = crate::v5::ops::state::find_repo(existing, &name) else {
                yield not_found_event(format!("repo '{name}' not found under org '{org}'")); return;
            };
            // Flip URL forms in org yaml.
            let mut updated = existing.clone();
            if let Some(mr) = crate::v5::ops::state::find_repo_mut(&mut updated, &name) {
                for r in &mut mr.remotes {
                    let new_url = flip_transport(r.url.as_str(), &transport);
                    r.url = crate::v5::config::RemoteUrl::from(new_url.as_str());
                }
            }
            let orgs_dir = config_dir.join("orgs");
            if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &updated) {
                yield cfg_error_event(e); return;
            }
            // Update .git/config if a path was given.
            if let Some(p) = path.as_ref() {
                let dir = std::path::PathBuf::from(p);
                if let Some(mr) = crate::v5::ops::state::find_repo_mut(&mut updated.clone(), &name) {
                    if let Some(first) = mr.remotes.first() {
                        let _ = crate::v5::ops::git::set_remote_url(&dir, "origin", first.url.as_str());
                    }
                }
            }
            yield RepoEvent::TransportSet {
                reference: RepoRefWire { org, name },
                transport,
            };
        }
    }

    // ==================================================================
    // V5PARITY-6: rename / set_default_branch / set_archived.
    // ==================================================================

    #[plexus_macros::method(params(
        org = "Org name",
        name = "Current repo name",
        new_name = "New repo name on forge AND in yaml"
    ))]
    pub async fn rename(
        &self,
        org: String,
        name: String,
        new_name: String,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            if org.is_empty() || name.is_empty() || new_name.is_empty() {
                yield validation_event("missing required parameter 'org', 'name', or 'new_name'"); return;
            }
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l, Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(existing) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found")); return;
            };
            let Some(repo) = crate::v5::ops::state::find_repo(existing, &name) else {
                yield not_found_event(format!("repo '{name}' not found under org '{org}'")); return;
            };
            // V5PARITY-34: rename hits the canonical remote in scope.
            let Some(first) = crate::v5::ops::repo::canonical_remote_in_scope(
                repo, &loaded.global.provider_map,
            ) else {
                if crate::v5::ops::repo::all_remotes_excluded(repo, &loaded.global.provider_map) {
                    yield RepoEvent::Error {
                        code: Some("forge_excluded".into()), error_class: None,
                        message: format!("repo '{name}' has remotes but `forges` scope excludes all"),
                    };
                } else {
                    yield validation_event(format!("repo '{name}' has no remotes"));
                }
                return;
            };
            let resolver = YamlSecretStore::new(&config_dir);
            let token_ref = crate::v5::ops::repo::token_ref_for(existing);
            let fallback_token_ref = Some(crate::v5::ops::repo::default_token_ref_for(existing));
            let repo_ref = RepoRef {
                org: OrgName::from(org.as_str()),
                name: RepoName::from(name.as_str()),
            };
            // Forge call first.
            if let Err(e) = crate::v5::ops::repo::rename_on_forge(
                first, &repo_ref, &new_name, &loaded.global.provider_map, &resolver, token_ref, fallback_token_ref.clone(),
            ).await {
                yield RepoEvent::Error {
                    code: Some(e.class.as_str().into()),
                    error_class: Some(e.class.as_str().into()),
                    message: e.message,
                };
                return;
            }
            // Update org yaml: rename the repo entry. Remote URLs that
            // include `/<old_name>` get path-rewritten too.
            let mut updated = existing.clone();
            if let Some(mr) = crate::v5::ops::state::find_repo_mut(&mut updated, &name) {
                mr.name = RepoName::from(new_name.as_str());
                for r in &mut mr.remotes {
                    let new_url = r.url.as_str()
                        .replace(&format!("/{name}.git"), &format!("/{new_name}.git"))
                        .replace(&format!("/{name}/"), &format!("/{new_name}/"));
                    r.url = crate::v5::config::RemoteUrl::from(new_url.as_str());
                }
            }
            let orgs_dir = config_dir.join("orgs");
            if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &updated) {
                yield cfg_error_event(e); return;
            }
            // Walk every workspace yaml; rewrite refs that pointed at the old name.
            let ws_dir = config_dir.join("workspaces");
            if ws_dir.is_dir() {
                if let Ok(all_ws) = crate::v5::ops::state::load_workspaces(&ws_dir) {
                    for (_, mut ws) in all_ws {
                        let mut changed = false;
                        for entry in &mut ws.repos {
                            match entry {
                                crate::v5::config::WorkspaceRepo::Shorthand(s) => {
                                    if *s == format!("{org}/{name}") {
                                        *s = format!("{org}/{new_name}");
                                        changed = true;
                                    }
                                }
                                crate::v5::config::WorkspaceRepo::Object { reference, .. } => {
                                    if reference.org.as_str() == org && reference.name.as_str() == name {
                                        reference.name = RepoName::from(new_name.as_str());
                                        changed = true;
                                    }
                                }
                            }
                        }
                        if changed {
                            let _ = crate::v5::ops::state::save_workspace(&ws_dir, &ws);
                        }
                    }
                }
            }
            yield RepoEvent::RepoRenamed {
                old_ref: RepoRefWire { org: org.clone(), name: name.clone() },
                new_ref: RepoRefWire { org, name: new_name },
            };
        }
    }

    /// Move ONE repo to a different (already-registered) org: relocate
    /// its org-membership entry, flip the local `.hyperforge/config.toml`
    /// `org`, and retarget its remote URLs. The target org must already
    /// exist — this never creates it. The local `config.toml` is
    /// git-tracked, so it will show dirty afterwards; the caller commits.
    #[plexus_macros::method(params(
        org = "Current org name",
        name = "Repo name",
        new_org = "Target org (must already be registered)",
        path = "Optional local checkout dir to flip config.toml + git remotes",
        dry_run = "Preview only (default true); --dry-run false writes"
    ))]
    pub async fn migrate_org(
        &self,
        org: String,
        name: String,
        new_org: String,
        path: Option<String>,
        dry_run: Option<Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let dry = dry_run.as_ref().map_or(true, |v| to_bool(v, true));
            if org.is_empty() || name.is_empty() || new_org.is_empty() {
                yield validation_event("missing required parameter 'org', 'name', or 'new_org'");
                return;
            }
            if new_org == org {
                yield validation_event("'new_org' must differ from 'org'");
                return;
            }
            let dir_opt = path
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from);
            // The generic migrate-org command assumes github as the
            // migrating forge (the fleet's only user→org mover); doctor/heal
            // pass the actual resolved remote's provider. HYPE-6.
            match migrate_one(&config_dir, &org, &name, &new_org, ProviderKind::Github, dir_opt.as_deref(), dry) {
                Ok(outcome) => {
                    yield RepoEvent::RepoMigrated {
                        reference: RepoRefWire { org: new_org.clone(), name: name.clone() },
                        old_org: org,
                        new_org,
                        local_path: outcome.local_path,
                        dry_run: dry,
                    };
                }
                Err(msg) => {
                    let (code, body) = msg
                        .split_once(": ")
                        .map_or(("validation", msg.as_str()), |(c, b)| (c, b));
                    yield RepoEvent::Error {
                        code: Some(code.to_string()),
                        error_class: matches!(code, "not_found" | "org_not_found")
                            .then(|| code.to_string()),
                        message: body.to_string(),
                    };
                }
            }
        }
    }

    /// HYPE-6: resolve each registered repo's canonical owner on the forge
    /// and report divergence; with `heal`, repair it through the
    /// provider-scoped `migrate_one`. Read-only by default; `heal` acts,
    /// `dry_run` previews. Owner-aliases (HYPE-5) are reported clean.
    #[plexus_macros::method(params(
        org = "Org whose repos to check against the forge",
        name = "Optional single repo to limit scope",
        heal = "Repair divergences via migrate_one (default: report only)",
        path = "Optional local checkout dir to also flip on heal",
        dry_run = "With heal: preview only (default true); --dry-run false repairs"
    ))]
    pub async fn doctor(
        &self,
        org: String,
        name: Option<String>,
        heal: Option<Value>,
        path: Option<String>,
        dry_run: Option<Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let do_heal = heal.as_ref().is_some_and(|v| to_bool(v, false));
            let dry = dry_run.as_ref().map_or(true, |v| to_bool(v, true));
            if org.is_empty() {
                yield validation_event("missing required parameter 'org'");
                return;
            }
            let loaded = match load_all(&config_dir) {
                Ok(l) => l,
                Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(org_cfg) = loaded.orgs.get(&OrgName::from(org.as_str())).cloned() else {
                yield not_found_event(format!("org '{org}' not found"));
                return;
            };
            let dir_opt = path
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from);
            let resolver = YamlSecretStore::new(&config_dir);
            let repos: Vec<OrgRepo> = match name.as_deref() {
                Some(only) => org_cfg.repos.iter().filter(|r| r.name.as_str() == only).cloned().collect(),
                None => org_cfg.repos.clone(),
            };
            if let Some(only) = name.as_deref() {
                if repos.is_empty() {
                    yield not_found_event(format!("repo '{only}' not found under org '{org}'"));
                    return;
                }
            }
            let mut checked = 0u32;
            let mut diverged = 0u32;
            let mut healed = 0u32;
            let mut renames: Vec<(String, String)> = Vec::new();
            for repo in &repos {
                checked += 1;
                let (events, rename) = doctor_one(
                    &config_dir, &org, &org_cfg, repo, &loaded.global,
                    &resolver, do_heal, dir_opt.as_deref(), dry,
                ).await;
                for e in &events {
                    if let RepoEvent::RepoDoctorEntry { verdict, .. } = e {
                        if verdict == "diverged" { diverged += 1; }
                    }
                    if matches!(e, RepoEvent::RepoHealed { .. }) { healed += 1; }
                }
                for e in events { yield e; }
                if let Some(r) = rename { renames.push(r); }
            }
            let renames_path = if do_heal && !dry && !renames.is_empty() {
                write_rename_map(&config_dir, &renames).ok().map(|p| p.display().to_string())
            } else {
                None
            };
            yield RepoEvent::RepoDoctorSummary { org, checked, diverged, healed, renames_path };
        }
    }

    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        branch = "Default branch"
    ))]
    pub async fn set_default_branch(
        &self,
        org: String,
        name: String,
        branch: String,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            if org.is_empty() || name.is_empty() || branch.is_empty() {
                yield validation_event("missing required parameter"); return;
            }
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l, Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(existing) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found")); return;
            };
            let Some(repo) = crate::v5::ops::state::find_repo(existing, &name) else {
                yield not_found_event(format!("repo '{name}' not found")); return;
            };
            // V5PARITY-34: canonical remote in scope.
            let Some(first) = crate::v5::ops::repo::canonical_remote_in_scope(
                repo, &loaded.global.provider_map,
            ) else {
                if crate::v5::ops::repo::all_remotes_excluded(repo, &loaded.global.provider_map) {
                    yield RepoEvent::Error {
                        code: Some("forge_excluded".into()), error_class: None,
                        message: format!("repo '{name}' has remotes but `forges` scope excludes all"),
                    };
                } else {
                    yield validation_event("no remotes");
                }
                return;
            };
            let resolver = YamlSecretStore::new(&config_dir);
            let token_ref = crate::v5::ops::repo::token_ref_for(existing);
            let fallback_token_ref = Some(crate::v5::ops::repo::default_token_ref_for(existing));
            let repo_ref = RepoRef {
                org: OrgName::from(org.as_str()),
                name: RepoName::from(name.as_str()),
            };
            let mut fields: MetadataFields = std::collections::BTreeMap::new();
            fields.insert(DriftFieldKind::DefaultBranch, serde_json::Value::String(branch.clone()));
            if let Err(e) = crate::v5::ops::repo::write_metadata_on_forge(
                first, &repo_ref, &fields, &loaded.global.provider_map, &resolver, token_ref, fallback_token_ref.clone(),
            ).await {
                yield RepoEvent::Error {
                    code: Some(e.class.as_str().into()),
                    error_class: Some(e.class.as_str().into()),
                    message: e.message,
                };
                return;
            }
            // Mirror to local metadata.
            let mut updated = existing.clone();
            if let Some(mr) = crate::v5::ops::state::find_repo_mut(&mut updated, &name) {
                let md = mr.metadata.get_or_insert_with(RepoMetadataLocal::default);
                md.default_branch = Some(branch.clone());
            }
            let orgs_dir = config_dir.join("orgs");
            if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &updated) {
                yield cfg_error_event(e); return;
            }
            yield RepoEvent::DefaultBranchSet {
                reference: RepoRefWire { org, name },
                branch,
            };
        }
    }

    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        archived = "true to archive, false to unarchive"
    ))]
    pub async fn set_archived(
        &self,
        org: String,
        name: String,
        archived: Option<Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            if org.is_empty() || name.is_empty() {
                yield validation_event("missing required parameter"); return;
            }
            let target = archived.as_ref().is_some_and(|v| to_bool(v, false));
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l, Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(existing) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found")); return;
            };
            let Some(repo) = crate::v5::ops::state::find_repo(existing, &name) else {
                yield not_found_event(format!("repo '{name}' not found")); return;
            };
            // V5PARITY-34: canonical remote in scope.
            let Some(first) = crate::v5::ops::repo::canonical_remote_in_scope(
                repo, &loaded.global.provider_map,
            ) else {
                if crate::v5::ops::repo::all_remotes_excluded(repo, &loaded.global.provider_map) {
                    yield RepoEvent::Error {
                        code: Some("forge_excluded".into()), error_class: None,
                        message: format!("repo '{name}' has remotes but `forges` scope excludes all"),
                    };
                } else {
                    yield validation_event("no remotes");
                }
                return;
            };
            let resolver = YamlSecretStore::new(&config_dir);
            let token_ref = crate::v5::ops::repo::token_ref_for(existing);
            let fallback_token_ref = Some(crate::v5::ops::repo::default_token_ref_for(existing));
            let repo_ref = RepoRef {
                org: OrgName::from(org.as_str()),
                name: RepoName::from(name.as_str()),
            };
            let mut fields: MetadataFields = std::collections::BTreeMap::new();
            fields.insert(DriftFieldKind::Archived, serde_json::Value::Bool(target));
            if let Err(e) = crate::v5::ops::repo::write_metadata_on_forge(
                first, &repo_ref, &fields, &loaded.global.provider_map, &resolver, token_ref, fallback_token_ref.clone(),
            ).await {
                yield RepoEvent::Error {
                    code: Some(e.class.as_str().into()),
                    error_class: Some(e.class.as_str().into()),
                    message: e.message,
                };
                return;
            }
            let mut updated = existing.clone();
            if let Some(mr) = crate::v5::ops::state::find_repo_mut(&mut updated, &name) {
                let md = mr.metadata.get_or_insert_with(RepoMetadataLocal::default);
                md.archived = Some(target);
            }
            let orgs_dir = config_dir.join("orgs");
            if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &updated) {
                yield cfg_error_event(e); return;
            }
            yield RepoEvent::ArchivedSet {
                reference: RepoRefWire { org, name },
                archived: target,
            };
        }
    }

    // ==================================================================
    // V5PARITY-4: analytics (size, loc, large_files).
    // `dirty` is defined above and reused by workspace aggregates;
    // D13 keeps is_dirty in ops::git so this ticket does not
    // reintroduce a second implementation.
    // ==================================================================

    #[plexus_macros::method(params(path = "Repo checkout directory"))]
    pub async fn size(
        &self,
        path: String,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        stream! {
            if path.is_empty() {
                yield validation_event("missing required parameter 'path'"); return;
            }
            let dir = std::path::PathBuf::from(&path);
            match crate::v5::ops::analytics::repo_size(&dir) {
                Ok(s) => yield RepoEvent::RepoSizeSummary {
                    path, bytes: s.bytes, file_count: s.file_count,
                },
                Err(e) => yield RepoEvent::Error {
                    code: Some(e.code().into()), error_class: None, message: e.to_string(),
                },
            }
        }
    }

    #[plexus_macros::method(params(path = "Repo checkout directory"))]
    pub async fn loc(
        &self,
        path: String,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        stream! {
            if path.is_empty() {
                yield validation_event("missing required parameter 'path'"); return;
            }
            let dir = std::path::PathBuf::from(&path);
            match crate::v5::ops::analytics::repo_loc(&dir) {
                Ok(m) => {
                    let total: u64 = m.values().sum();
                    yield RepoEvent::RepoLocSummary { path, by_language: m, total };
                }
                Err(e) => yield RepoEvent::Error {
                    code: Some(e.code().into()), error_class: None, message: e.to_string(),
                },
            }
        }
    }

    #[plexus_macros::method(params(
        path = "Repo checkout directory",
        threshold = "Threshold in KB (default: 100)"
    ))]
    pub async fn large_files(
        &self,
        path: String,
        threshold: Option<u64>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        stream! {
            if path.is_empty() {
                yield validation_event("missing required parameter 'path'"); return;
            }
            let threshold_bytes = threshold.unwrap_or(100) * 1024;
            let dir = std::path::PathBuf::from(&path);
            match crate::v5::ops::analytics::large_files(&dir, threshold_bytes) {
                Ok(items) => {
                    let count = items.len() as u64;
                    for it in items {
                        yield RepoEvent::LargeFile { path: it.path, size: it.size };
                    }
                    yield RepoEvent::LargeFilesSummary {
                        path, threshold_bytes, count,
                    };
                }
                Err(e) => yield RepoEvent::Error {
                    code: Some(e.code().into()), error_class: None, message: e.to_string(),
                },
            }
        }
    }

    // ==================================================================
    // V5PARITY-5: per-repo SSH key wiring.
    // ==================================================================

    #[plexus_macros::method(params(
        path = "Repo checkout directory",
        key = "Filesystem path to the SSH private key (~ expanded)",
        org = "Optional org to persist the key on (adds a ssh_key credential)",
        name = "Optional repo name (reserved for per-repo override)",
        persist_to_org = "If true, also add the key to the org yaml (default: false)"
    ))]
    pub async fn set_ssh_key(
        &self,
        path: String,
        key: String,
        org: Option<String>,
        name: Option<String>,
        persist_to_org: Option<serde_json::Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            if path.is_empty() || key.is_empty() {
                yield validation_event("missing required parameter 'path' or 'key'");
                return;
            }
            let persist = persist_to_org.as_ref().is_some_and(|v| match v {
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::String(s) => matches!(s.as_str(), "true" | "1" | "yes"),
                _ => false,
            });
            let key_path = expand_tilde(&key);
            if !key_path.exists() {
                yield RepoEvent::Error {
                    code: Some("invalid_key".into()),
                    error_class: None,
                    message: format!("ssh key not found: {}", key_path.display()),
                };
                return;
            }
            let dir = std::path::PathBuf::from(&path);
            if let Err(e) = crate::v5::ops::git::set_ssh_command(&dir, &key_path) {
                yield RepoEvent::Error {
                    code: Some(e.code().into()), error_class: None, message: e.to_string(),
                };
                return;
            }
            let mut persisted = false;
            if persist {
                if let Some(org_name) = org.as_deref() {
                    let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                        Ok(l) => l, Err(e) => { yield cfg_error_event(e); return; }
                    };
                    if let Some(existing) = loaded.orgs.get(&OrgName::from(org_name)) {
                        let mut updated = existing.clone();
                        updated.forge.credentials.retain(|c| !matches!(c.cred_type, CredentialType::SshKey));
                        updated.forge.credentials.push(crate::v5::config::CredentialEntry {
                            key: key_path.display().to_string(),
                            cred_type: CredentialType::SshKey,
                        });
                        let orgs_dir = config_dir.join("orgs");
                        if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &updated) {
                            yield cfg_error_event(e); return;
                        }
                        persisted = true;
                    }
                }
            }
            yield RepoEvent::RepoSshKeySet {
                path,
                key: key_path.display().to_string(),
                org,
                name,
                persisted,
            };
        }
    }

    // ==================================================================
    // V5PARITY-25: adopt an existing local checkout.
    // ==================================================================

    #[plexus_macros::method(params(
        target_path = "Path to an existing checkout (named target_path because synapse path-expands params named exactly 'path')",
        org = "Override the auto-derived org name (default: derived from origin URL via provider_map)",
        repo_name = "Override the auto-derived repo name (default: last URL segment minus .git)",
        init = "Run repos.init to write .hyperforge/config.toml (default: true)"
    ))]
    pub async fn register(
        &self,
        target_path: String,
        org: Option<String>,
        repo_name: Option<String>,
        init: Option<serde_json::Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            if target_path.is_empty() {
                yield validation_event("missing required parameter 'target_path'");
                return;
            }
            let do_init = init.as_ref().map_or(true, |v| to_bool(v, true));
            let dir = std::path::PathBuf::from(&target_path);
            if !dir.is_dir() {
                yield validation_event(format!("not a directory: {}", dir.display()));
                return;
            }
            // Read origin URL via ops::git (V5PARITY-15).
            let origin = match crate::v5::ops::git::read_origin_url(&dir) {
                Ok(Some(u)) => u,
                Ok(None) => {
                    yield validation_event(format!("no origin remote in {}", dir.display()));
                    return;
                }
                Err(e) => {
                    yield RepoEvent::Error {
                        code: Some(e.code().into()), error_class: None, message: e.to_string(),
                    };
                    return;
                }
            };
            // Parse host/owner/name.
            let Some((host, derived_owner, derived_name)) =
                crate::v5::adapters::parse_remote_url(&origin)
            else {
                yield validation_event(format!("could not parse remote URL: {origin}"));
                return;
            };
            // Resolve provider via the global provider_map.
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l, Err(e) => { yield cfg_error_event(e); return; }
            };
            let domain = DomainName::from(host.as_str());
            let provider = match loaded.global.provider_map.get(&domain) {
                Some(p) => *p,
                None => {
                    yield validation_event(format!("no provider for host '{host}'; add it to config.yaml provider_map"));
                    return;
                }
            };
            let _ = provider; // recorded on Remote below if/when needed
            // HYPE-5: file under the CANONICAL owner. When a repo moved
            // user→org the URL-derived owner is the old (alias) user; the
            // registry entry must land under the canonical org, not the raw
            // URL-derived string. Explicit `org` is canonicalized too.
            let org_name =
                loaded.global.canonical_owner(org.as_deref().unwrap_or(&derived_owner));
            let name = repo_name.as_deref().unwrap_or(&derived_name).to_string();
            let org_key = OrgName::from(org_name.as_str());
            let mut org_cfg = match loaded.orgs.get(&org_key).cloned() {
                Some(c) => c,
                None => {
                    yield not_found_event(format!("org '{org_name}' not configured; run orgs.bootstrap first"));
                    return;
                }
            };
            // Collect ALL remotes from the checkout's git config (not
            // just origin). Pulls these via git2 for free locally.
            let observed_remotes = collect_all_remotes(&dir).unwrap_or_else(|_| {
                vec![crate::v5::config::Remote {
                    url: crate::v5::config::RemoteUrl::from(origin.as_str()),
                    provider: None,
                }]
            });
            // Conflict check: existing entry with same name but different remotes.
            if let Some(existing) = org_cfg.repos.iter().find(|r| r.name.as_str() == name) {
                let existing_urls: std::collections::BTreeSet<&str> =
                    existing.remotes.iter().map(|r| r.url.as_str()).collect();
                let observed_urls: std::collections::BTreeSet<&str> =
                    observed_remotes.iter().map(|r| r.url.as_str()).collect();
                if existing_urls != observed_urls {
                    yield RepoEvent::RepoConflict {
                        reference: RepoRefWire { org: org_name, name },
                        existing_remotes: existing_urls.into_iter().map(String::from).collect(),
                        observed_remotes: observed_urls.into_iter().map(String::from).collect(),
                    };
                    return;
                }
                // Same remotes — idempotent, no write needed but emit success.
            } else {
                // Add the new entry.
                org_cfg.repos.push(crate::v5::config::OrgRepo {
                    name: RepoName::from(name.as_str()),
                    remotes: observed_remotes.clone(),
                    forges: None,
                    metadata: None,
                });
                let orgs_dir = config_dir.join("orgs");
                if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &org_cfg) {
                    yield cfg_error_event(e); return;
                }
            }
            // Optionally write `.hyperforge/config.toml` via repos.init.
            let init_done = if do_init {
                let hf_dir = dir.join(".hyperforge");
                let _ = std::fs::create_dir_all(&hf_dir);
                let toml = format!(
                    "repo_name = \"{name}\"\norg = \"{org_name}\"\nforges = [\"{provider_str}\"]\n",
                    provider_str = match provider {
                        ProviderKind::Github => "github",
                        ProviderKind::Codeberg => "codeberg",
                        ProviderKind::Gitlab => "gitlab",
                    },
                );
                let _ = std::fs::write(hf_dir.join("config.toml"), toml);
                true
            } else { false };
            let remotes_wire: Vec<RemoteWire> = observed_remotes.iter().map(|r| RemoteWire {
                url: r.url.as_str().to_string(),
                provider: match provider {
                    ProviderKind::Github => "github".into(),
                    ProviderKind::Codeberg => "codeberg".into(),
                    ProviderKind::Gitlab => "gitlab".into(),
                },
            }).collect();
            yield RepoEvent::RepoRegistered {
                reference: RepoRefWire { org: org_name, name },
                path: target_path,
                remotes: remotes_wire,
                init_done,
            };
        }
    }

    // ==================================================================
    // V5PARITY-34: sync .hyperforge/config.toml ↔ org yaml.
    // ==================================================================

    #[plexus_macros::method(params(
        target_path = "Path to the checkout (named target_path because synapse path-expands params named exactly 'path')",
        mode = "push (yaml → file) | pull (file → yaml; default)"
    ))]
    pub async fn sync_config(
        &self,
        target_path: String,
        mode: Option<String>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            if target_path.is_empty() {
                yield validation_event("missing required parameter 'target_path'"); return;
            }
            let mode = mode.as_deref().unwrap_or("pull");
            if mode != "pull" && mode != "push" {
                yield validation_event(format!("unknown mode '{mode}'; expected 'pull' or 'push'"));
                return;
            }
            let dir = std::path::PathBuf::from(&target_path);
            let local_path = dir.join(".hyperforge").join("config.toml");

            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l, Err(e) => { yield cfg_error_event(e); return; }
            };

            // Read the per-repo file to find the (org, name) identity.
            let Ok(Some(local)) = crate::v5::ops::fs::read_hyperforge_config(&dir) else {
                yield not_found_event(format!(
                    "no .hyperforge/config.toml in {} (run repos.init first)",
                    dir.display()
                ));
                return;
            };
            let org_name = local.org.clone();
            let repo_name = local.repo_name.clone();
            let Some(existing) = loaded.orgs.get(&org_name).cloned() else {
                yield not_found_event(format!("org '{}' not found", org_name.as_str())); return;
            };
            if crate::v5::ops::state::find_repo(&existing, &repo_name).is_none() {
                yield not_found_event(format!("repo '{repo_name}' not found in org '{}'", org_name.as_str()));
                return;
            };

            match mode {
                "pull" => {
                    // file → yaml.
                    let mut updated = existing.clone();
                    let mut changed = false;
                    if let Some(mr) = crate::v5::ops::state::find_repo_mut(&mut updated, &repo_name) {
                        let new_forges = if local.forges.is_empty() {
                            None
                        } else {
                            Some(local.forges.clone())
                        };
                        if mr.forges != new_forges {
                            mr.forges = new_forges;
                            changed = true;
                        }
                    }
                    if changed {
                        let orgs_dir = config_dir.join("orgs");
                        if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &updated) {
                            yield cfg_error_event(e); return;
                        }
                    }
                    yield RepoEvent::ConfigSynced {
                        reference: RepoRefWire {
                            org: org_name.as_str().to_string(),
                            name: repo_name.as_str().to_string(),
                        },
                        mode: "pull".into(),
                        local_path: local_path.display().to_string(),
                        changed,
                    };
                }
                "push" => {
                    // yaml → file.
                    let mr = crate::v5::ops::state::find_repo(&existing, &repo_name).unwrap();
                    let cfg_to_write = crate::v5::ops::fs::HyperforgeRepoConfig {
                        repo_name: repo_name.as_str().to_string(),
                        org: org_name.clone(),
                        forges: mr.forges.clone().unwrap_or_else(Vec::new),
                        default_branch: local.default_branch.clone(),
                        visibility: local.visibility.clone(),
                        description: local.description.clone(),
                    };
                    let _ = std::fs::create_dir_all(dir.join(".hyperforge"));
                    if let Err(e) = crate::v5::ops::fs::write_hyperforge_config(&dir, &cfg_to_write, true) {
                        yield RepoEvent::Error {
                            code: Some(e.code().into()), error_class: None, message: e.to_string(),
                        };
                        return;
                    }
                    yield RepoEvent::ConfigSynced {
                        reference: RepoRefWire {
                            org: org_name.as_str().to_string(),
                            name: repo_name.as_str().to_string(),
                        },
                        mode: "push".into(),
                        local_path: local_path.display().to_string(),
                        changed: true,
                    };
                }
                _ => unreachable!(),
            }
        }
    }

    // ==================================================================
    // V5PARITY-35: typed RPC for scoping a repo's forges.
    // ==================================================================

    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        forges = "Comma-separated providers (github,codeberg,gitlab) | 'none' (= []) | 'unset' (= null)",
        dry_run = "Preview without writing"
    ))]
    pub async fn set_forges(
        &self,
        org: String,
        name: String,
        forges: String,
        dry_run: Option<serde_json::Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            if org.is_empty() || name.is_empty() {
                yield validation_event("missing required parameter 'org' or 'name'");
                return;
            }
            let dry = dry_run.as_ref().is_some_and(|v| to_bool(v, false));
            // Parse the special-form forges argument.
            let target = match parse_forges_arg(&forges) {
                Ok(t) => t,
                Err(e) => { yield validation_event(e); return; }
            };
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l, Err(e) => { yield cfg_error_event(e); return; }
            };
            let Some(existing) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield not_found_event(format!("org '{org}' not found")); return;
            };
            let Some(_repo) = crate::v5::ops::state::find_repo(existing, &name) else {
                yield not_found_event(format!("repo '{name}' not found in org '{org}'")); return;
            };
            let mut updated = existing.clone();
            let mut changed = false;
            if let Some(mr) = crate::v5::ops::state::find_repo_mut(&mut updated, &name) {
                if mr.forges != target {
                    mr.forges = target.clone();
                    changed = true;
                }
            }
            if changed && !dry {
                let orgs_dir = config_dir.join("orgs");
                if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &updated) {
                    yield cfg_error_event(e); return;
                }
            }
            // Best-effort: write through to .hyperforge/config.toml if a
            // checkout for this repo can be found under any workspace path.
            let local_path = if dry { None } else {
                write_through_repo_config(&config_dir, &org, &name, target.as_deref(), &loaded)
            };
            yield RepoEvent::ForgesSet {
                reference: RepoRefWire { org, name },
                forges: target.map(|v| v.into_iter().map(|p| match p {
                    ProviderKind::Github => "github".to_string(),
                    ProviderKind::Codeberg => "codeberg".to_string(),
                    ProviderKind::Gitlab => "gitlab".to_string(),
                }).collect()),
                changed,
                dry_run: dry,
                local_path,
            };
        }
    }

    // ==================================================================
    // V5PARITY-36: typed retire/migrate across forges.
    // ==================================================================

    #[plexus_macros::method(params(
        org = "Source org name",
        name = "Source repo name",
        to = "Destination as <provider>/<org>[/<rename>] (e.g., codeberg/hyperslop)",
        retire = "After migrate succeeds, scope source to forges=[] (default true)",
        archive_source = "Set archived=true on the source forge BEFORE retiring (default false)",
        mirror = "Continuous pull mirror at destination instead of one-shot copy (default false)",
        private = "Destination visibility (default true; matches archive intent)",
        description = "Optional description for the destination repo",
        dry_run = "Preview without writing"
    ))]
    pub async fn migrate(
        &self,
        org: String,
        name: String,
        to: String,
        retire: Option<serde_json::Value>,
        archive_source: Option<serde_json::Value>,
        mirror: Option<serde_json::Value>,
        private: Option<serde_json::Value>,
        description: Option<String>,
        dry_run: Option<serde_json::Value>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            if org.is_empty() || name.is_empty() || to.is_empty() {
                yield validation_event("missing required parameter 'org', 'name', or 'to'");
                return;
            }
            let dry = dry_run.as_ref().is_some_and(|v| to_bool(v, false));
            let do_retire = retire.as_ref().map_or(true, |v| to_bool(v, true));
            let do_archive = archive_source.as_ref().is_some_and(|v| to_bool(v, false));
            let do_mirror = mirror.as_ref().is_some_and(|v| to_bool(v, false));
            let is_private = private.as_ref().map_or(true, |v| to_bool(v, true));

            // Parse destination spec.
            let parts: Vec<&str> = to.splitn(3, '/').collect();
            if parts.len() < 2 {
                yield validation_event(format!("--to must be '<provider>/<org>[/<rename>]'; got '{to}'"));
                return;
            }
            let dest_provider = match parts[0].to_ascii_lowercase().as_str() {
                "github" => ProviderKind::Github,
                "codeberg" => ProviderKind::Codeberg,
                "gitlab" => ProviderKind::Gitlab,
                other => { yield validation_event(format!("unknown provider '{other}'")); return; }
            };
            let dest_org = parts[1].to_string();
            let dest_name = parts.get(2).map(|s| s.to_string()).unwrap_or_else(|| name.clone());

            let source_wire = RepoRefWire { org: org.clone(), name: name.clone() };
            let dest_wire = RepoRefWire { org: dest_org.clone(), name: dest_name.clone() };

            // ---- precheck: source + dest org both configured. ----
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l, Err(e) => {
                    yield RepoEvent::MigrateFailed {
                        reference: source_wire.clone(), stage: "precheck".into(),
                        message: format!("load config: {e}"),
                    };
                    return;
                }
            };
            let Some(source_org_cfg) = loaded.orgs.get(&OrgName::from(org.as_str())) else {
                yield RepoEvent::MigrateFailed {
                    reference: source_wire.clone(), stage: "precheck".into(),
                    message: format!("source org '{org}' not configured"),
                };
                return;
            };
            let Some(source_repo) = crate::v5::ops::state::find_repo(source_org_cfg, &name) else {
                yield RepoEvent::MigrateFailed {
                    reference: source_wire.clone(), stage: "precheck".into(),
                    message: format!("source repo '{name}' not in org '{org}'"),
                };
                return;
            };
            let Some(dest_org_cfg) = loaded.orgs.get(&OrgName::from(dest_org.as_str())) else {
                yield RepoEvent::MigrateFailed {
                    reference: source_wire.clone(), stage: "precheck".into(),
                    message: format!("dest org '{dest_org}' not configured (run orgs.bootstrap first)"),
                };
                return;
            };
            if dest_org_cfg.forge.provider != dest_provider {
                yield RepoEvent::MigrateFailed {
                    reference: source_wire.clone(), stage: "precheck".into(),
                    message: format!(
                        "dest provider mismatch: --to says '{}' but org '{}' is provider {:?}",
                        parts[0], dest_org, dest_org_cfg.forge.provider
                    ),
                };
                return;
            };

            // ---- source_resolve: pick the source remote URL + auth. ----
            let Some(source_remote) = source_repo.canonical_remote() else {
                yield RepoEvent::MigrateFailed {
                    reference: source_wire.clone(), stage: "source_resolve".into(),
                    message: format!("source repo '{name}' has no remotes"),
                };
                return;
            };
            let source_url = source_remote.url.as_str().to_string();
            let resolver = YamlSecretStore::new(&config_dir);
            // Resolve source token via the same chain repos.sync uses
            // (V5PARITY-24 explicit-then-default).
            let source_token = {
                let auth = crate::v5::adapters::ForgeAuth {
                    token_ref: crate::v5::ops::repo::token_ref_for(source_org_cfg),
                    fallback_token_ref: Some(crate::v5::ops::repo::default_token_ref_for(source_org_cfg)),
                    resolver: &resolver,
                };
                auth.resolve_token().ok()
            };

            yield RepoEvent::MigrateStarted {
                reference: source_wire.clone(),
                source_url: source_url.clone(),
                dest_provider: parts[0].to_string(),
                dest_org: dest_org.clone(),
                dest_name: dest_name.clone(),
            };

            if dry {
                yield RepoEvent::MigrateDone {
                    source_ref: source_wire, dest_ref: dest_wire,
                    retired: do_retire, archived: do_archive, dry_run: true,
                };
                return;
            }

            // ---- forge_migrate: pull-clone source into dest. ----
            let dest_repo_ref = RepoRef {
                org: OrgName::from(dest_org.as_str()),
                name: RepoName::from(dest_name.as_str()),
            };
            let migrate_options = crate::v5::adapters::MigrateOptions {
                private: is_private,
                description: description.unwrap_or_else(|| format!("Retired from {}/{}.", org, name)),
                mirror: do_mirror,
            };
            let dest_token_ref = crate::v5::ops::repo::token_ref_for(dest_org_cfg);
            let dest_fallback = Some(crate::v5::ops::repo::default_token_ref_for(dest_org_cfg));
            let dest_remote_repo = match crate::v5::ops::repo::migrate_on_forge(
                dest_provider, &source_url, &dest_repo_ref, &migrate_options,
                source_token.as_deref(), &resolver, dest_token_ref, dest_fallback,
            ).await {
                Ok(r) => r,
                Err(e) => {
                    yield RepoEvent::MigrateFailed {
                        reference: source_wire.clone(), stage: "forge_migrate".into(),
                        message: format!("{}: {}", e.class.as_str(), e.message),
                    };
                    return;
                }
            };
            yield RepoEvent::ForgeMigrated {
                reference: source_wire.clone(),
                dest_url: dest_remote_repo.url.clone(),
            };

            // ---- v5_register: add dest to v5's destination org yaml. ----
            let mut updated_dest = dest_org_cfg.clone();
            let already = updated_dest.repos.iter().any(|r| r.name.as_str() == dest_name);
            if !already {
                updated_dest.repos.push(crate::v5::config::OrgRepo {
                    name: RepoName::from(dest_name.as_str()),
                    remotes: vec![crate::v5::config::Remote {
                        url: crate::v5::config::RemoteUrl::from(dest_remote_repo.url.as_str()),
                        provider: None,
                    }],
                    forges: None,
                    metadata: None,
                });
                let orgs_dir = config_dir.join("orgs");
                if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &updated_dest) {
                    yield RepoEvent::MigrateFailed {
                        reference: source_wire.clone(), stage: "v5_register".into(),
                        message: format!("save dest org yaml: {e}"),
                    };
                    return;
                }
            }

            // ---- archive: optionally archive on source BEFORE scoping. ----
            if do_archive {
                let mut updated_source = source_org_cfg.clone();
                if let Some(mr) = crate::v5::ops::state::find_repo_mut(&mut updated_source, &name) {
                    let md = mr.metadata.get_or_insert_with(crate::v5::config::RepoMetadataLocal::default);
                    md.archived = Some(true);
                }
                let orgs_dir = config_dir.join("orgs");
                if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &updated_source) {
                    yield RepoEvent::MigrateFailed {
                        reference: source_wire.clone(), stage: "archive".into(),
                        message: format!("save source org yaml after archive: {e}"),
                    };
                    return;
                }
                // Push archived to the source forge.
                let mut fields: crate::v5::adapters::MetadataFields = std::collections::BTreeMap::new();
                fields.insert(
                    crate::v5::adapters::DriftFieldKind::Archived,
                    serde_json::Value::Bool(true),
                );
                if let Err(e) = crate::v5::ops::repo::write_metadata_on_forge(
                    source_remote, &RepoRef {
                        org: OrgName::from(org.as_str()), name: RepoName::from(name.as_str()),
                    },
                    &fields, &loaded.global.provider_map, &resolver,
                    crate::v5::ops::repo::token_ref_for(source_org_cfg),
                    Some(crate::v5::ops::repo::default_token_ref_for(source_org_cfg)),
                ).await {
                    yield RepoEvent::MigrateFailed {
                        reference: source_wire.clone(), stage: "archive".into(),
                        message: format!("archive on source forge: {}", e.message),
                    };
                    return;
                }
            }

            // ---- retire: scope source to forges=[]. ----
            if do_retire {
                let mut updated_source = source_org_cfg.clone();
                if let Some(mr) = crate::v5::ops::state::find_repo_mut(&mut updated_source, &name) {
                    mr.forges = Some(Vec::new());
                }
                let orgs_dir = config_dir.join("orgs");
                if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &updated_source) {
                    yield RepoEvent::MigrateFailed {
                        reference: source_wire.clone(), stage: "retire".into(),
                        message: format!("save source org yaml: {e}"),
                    };
                    return;
                }
                // Best-effort: write through to .hyperforge/config.toml if checkout exists.
                let _ = write_through_repo_config(&config_dir, &org, &name, Some(&[]), &loaded);
            }

            yield RepoEvent::MigrateDone {
                source_ref: source_wire, dest_ref: dest_wire,
                retired: do_retire, archived: do_archive, dry_run: false,
            };
        }
    }
}

/// V5PARITY-35: parse the `forges` arg.
/// - `unset` → `None`              (legacy unscoped behavior; field removed)
/// - `none`  → `Some(vec![])`      (scoped to no forges)
/// - csv     → `Some(vec![...])`   (scoped to listed providers)
pub(crate) fn parse_forges_arg(raw: &str) -> Result<Option<Vec<ProviderKind>>, String> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("unset") || trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.eq_ignore_ascii_case("none") {
        return Ok(Some(Vec::new()));
    }
    let mut out: Vec<ProviderKind> = Vec::new();
    for part in trimmed.split(',') {
        let p = match part.trim().to_ascii_lowercase().as_str() {
            "github" => ProviderKind::Github,
            "codeberg" => ProviderKind::Codeberg,
            "gitlab" => ProviderKind::Gitlab,
            other => return Err(format!("unknown provider '{other}' in forges list")),
        };
        if !out.contains(&p) { out.push(p); }
    }
    Ok(Some(out))
}

/// V5PARITY-35: best-effort write-through to `.hyperforge/config.toml`.
/// Searches every workspace yaml for a member matching `(org, name)`;
/// uses the workspace path + member dir to locate the checkout. Writes
/// the file's `forges` to match the provided value. Returns the path
/// written, or `None` if no checkout was found.
pub(crate) fn write_through_repo_config(
    config_dir: &std::path::Path,
    org: &str,
    name: &str,
    target: Option<&[ProviderKind]>,
    loaded: &crate::v5::config::LoadedConfig,
) -> Option<String> {
    use crate::v5::config::WorkspaceRepo;
    let mut hits: Vec<std::path::PathBuf> = Vec::new();
    for ws in loaded.workspaces.values() {
        let ws_path = std::path::PathBuf::from(ws.path.as_str());
        for entry in &ws.repos {
            let (o, n, dir) = match entry {
                WorkspaceRepo::Shorthand(s) => match s.split_once('/') {
                    Some((a, b)) => (a.to_string(), b.to_string(), b.to_string()),
                    None => continue,
                },
                WorkspaceRepo::Object { reference, dir } => (
                    reference.org.as_str().to_string(),
                    reference.name.as_str().to_string(),
                    dir.clone(),
                ),
            };
            if o == org && n == name {
                hits.push(ws_path.join(dir));
            }
        }
    }
    let _ = config_dir; // kept for future use (e.g., reading workspace yaml directly)
    let dir = hits.into_iter().find(|p| p.is_dir())?;
    // Read existing file (if any), update forges, write back.
    let cfg_path = dir.join(".hyperforge").join("config.toml");
    let mut existing = crate::v5::ops::fs::read_hyperforge_config(&dir).ok().flatten()?;
    let new_forges: Vec<ProviderKind> = target.map(|s| s.to_vec()).unwrap_or_default();
    if existing.forges == new_forges {
        // No change needed.
        return Some(cfg_path.display().to_string());
    }
    existing.forges = new_forges;
    let _ = std::fs::create_dir_all(dir.join(".hyperforge"));
    crate::v5::ops::fs::write_hyperforge_config(&dir, &existing, true).ok()
        .map(|p| p.display().to_string())
}

/// V5PARITY-25 helper: enumerate every remote on a checkout via git2.
/// Falls back to `Err` if the dir isn't a real repo (caller may have
/// only origin from `read_origin_url`'s INI fallback).
fn collect_all_remotes(dir: &std::path::Path) -> Result<Vec<crate::v5::config::Remote>, ()> {
    use git2::Repository;
    let repo = Repository::open(dir).map_err(|_| ())?;
    let names = repo.remotes().map_err(|_| ())?;
    let mut origin_url: Option<String> = None;
    let mut others: Vec<String> = Vec::new();
    for name in names.iter().flatten() {
        if let Ok(remote) = repo.find_remote(name) {
            if let Some(url) = remote.url() {
                if name == "origin" {
                    origin_url = Some(url.to_string());
                } else {
                    others.push(url.to_string());
                }
            }
        }
    }
    let mut out: Vec<crate::v5::config::Remote> = Vec::new();
    if let Some(u) = origin_url {
        out.push(crate::v5::config::Remote {
            url: crate::v5::config::RemoteUrl::from(u.as_str()),
            provider: None,
        });
    }
    for u in others {
        out.push(crate::v5::config::Remote {
            url: crate::v5::config::RemoteUrl::from(u.as_str()),
            provider: None,
        });
    }
    if out.is_empty() { Err(()) } else { Ok(out) }
}

/// Resolve the SSH private-key path for an org. Returns the path on
/// the first `CredentialEntry { cred_type: SshKey }`. Consulted from
/// `repos.clone` (V5PARITY-5) and from `repos.set_ssh_key`.
fn ssh_key_for_org(org: &OrgConfig) -> Option<PathBuf> {
    org.forge.credentials.iter()
        .find(|c| matches!(c.cred_type, CredentialType::SshKey))
        .map(|c| expand_tilde(&c.key))
}

/// Expand a leading `~/` to `$HOME`. Non-tilde paths pass through.
fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

/// Flip a git URL's transport form. Best-effort: recognizes the
/// GitHub/GitLab/Codeberg patterns and rewrites; returns input
/// unchanged for URLs it can't interpret.
fn flip_transport(url: &str, to: &str) -> String {
    // git@host:org/name.git  ↔  https://host/org/name.git
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            if to == "https" {
                return format!("https://{host}/{path}");
            }
        }
    }
    if let Some(rest) = url.strip_prefix("https://") {
        if let Some((host, path)) = rest.split_once('/') {
            if to == "ssh" {
                return format!("git@{host}:{path}");
            }
        }
    }
    url.to_string()
}

// ---------------------------------------------------------------------
// Helpers internal to the methods above.
// ---------------------------------------------------------------------

fn repo_detail_event(
    org: String,
    name: String,
    repo: &OrgRepo,
    provider_map: &BTreeMap<DomainName, ProviderKind>,
) -> Result<RepoEvent, String> {
    let remotes: Result<Vec<RemoteWire>, String> = repo
        .remotes
        .iter()
        .map(|r| remote_to_wire(r, provider_map))
        .collect();
    let remotes = remotes?;
    Ok(RepoEvent::RepoDetail {
        reference: RepoRefWire { org, name },
        remotes,
        metadata: repo.metadata.clone(),
    })
}

fn metadata_from_local(local: &Option<RepoMetadataLocal>) -> MetadataFields {
    let mut out = MetadataFields::new();
    if let Some(m) = local {
        if let Some(v) = &m.default_branch {
            out.insert(DriftFieldKind::DefaultBranch, Value::String(v.clone()));
        }
        if let Some(v) = &m.description {
            out.insert(DriftFieldKind::Description, Value::String(v.clone()));
        }
        if let Some(v) = m.archived {
            out.insert(DriftFieldKind::Archived, Value::Bool(v));
        }
        if let Some(v) = &m.visibility {
            out.insert(DriftFieldKind::Visibility, Value::String(v.clone()));
        }
    }
    out
}

// V5LIFECYCLE-3: `compute_drift` relocated to `crate::v5::ops::repo`.
// No in-module callers remain after the migration.

// Silence unused-import lint if adapters are only used indirectly.
#[allow(dead_code)]
struct _KeepLinkedTypes(Arc<dyn SecretResolver>, ForgePortError, RemoteUrl);

#[cfg(test)]
mod migrate_tests {
    use super::*;
    use crate::v5::config::{ForgeBlock, OrgConfig};
    use futures::StreamExt;

    fn org_with_repo(name: &str, repos: Vec<OrgRepo>) -> OrgConfig {
        OrgConfig {
            name: OrgName::from(name),
            forge: ForgeBlock { provider: ProviderKind::Github, credentials: vec![] },
            mirrors: vec![],
            repos,
        }
    }

    fn ssh_remote(org: &str, repo: &str) -> Remote {
        Remote {
            url: RemoteUrl::from(format!("git@github.com:{org}/{repo}.git").as_str()),
            provider: Some(ProviderKind::Github),
        }
    }

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        crate::v5::ops::git::run_in(dir, args)
            .unwrap_or_else(|e| panic!("git {args:?} failed in {}: {e}", dir.display()));
    }

    /// HYPE-5 (adopt-uses-canonical): a checkout whose origin owner is the
    /// alias user `hypermemetic` must register under the CANONICAL org
    /// `hypermemetic-ai`, never under the raw URL-derived owner.
    #[tokio::test]
    async fn test_register_files_under_canonical_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().to_path_buf();
        let orgs_dir = config_dir.join("orgs");
        std::fs::create_dir_all(&orgs_dir).unwrap();

        // Canonical org exists (empty); the alias-user org does NOT.
        save_org(&orgs_dir, &org_with_repo("hypermemetic-ai", vec![])).unwrap();

        // Global config: provider_map for github.com + owner_aliases mapping
        // the canonical org to the old user alias. Written as a raw YAML
        // string (D13: no direct serde_yaml calls outside ops/config).
        std::fs::write(
            config_dir.join("config.yaml"),
            "provider_map:\n  github.com: github\nowner_aliases:\n  hypermemetic-ai:\n    - hypermemetic\n",
        )
        .unwrap();

        // A real checkout whose origin owner is the ALIAS user.
        let checkout = config_dir.join("widget");
        std::fs::create_dir_all(&checkout).unwrap();
        run_git(&checkout, &["init", "-q"]);
        run_git(
            &checkout,
            &["remote", "add", "origin", "git@github.com:hypermemetic/widget.git"],
        );

        let service = ReposService::with_config_dir(config_dir.clone());
        let events: Vec<RepoEvent> = service
            .register(
                checkout.display().to_string(),
                None,
                None,
                Some(serde_json::json!(false)),
            )
            .await
            .collect()
            .await;
        assert!(
            !events.iter().any(|e| matches!(e, RepoEvent::Error { .. })),
            "no error events expected, got {events:?}",
        );

        // Repo landed under the CANONICAL org, not the URL-derived alias.
        let reloaded = load_all(&config_dir).unwrap();
        let canonical = reloaded
            .orgs
            .get(&OrgName::from("hypermemetic-ai"))
            .expect("canonical org present");
        assert!(
            find_repo(canonical, "widget").is_some(),
            "repo must file under the canonical owner hypermemetic-ai",
        );
        assert!(
            reloaded.orgs.get(&OrgName::from("hypermemetic")).is_none(),
            "no alias-user org should be created",
        );
    }

    /// Mirrors the rename anti-story: set up two orgs + a repo under the
    /// source, migrate it, and assert the OrgRepo moved to the target
    /// org yaml with remotes retargeted and the local config.toml org
    /// flipped.
    #[tokio::test]
    async fn test_migrate_org_moves_membership_and_flips_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().to_path_buf();
        let orgs_dir = config_dir.join("orgs");
        std::fs::create_dir_all(&orgs_dir).unwrap();

        // Source org owns `widget`; target org starts empty.
        let repo = OrgRepo {
            name: RepoName::from("widget"),
            remotes: vec![ssh_remote("hypermemetic", "widget")],
            forges: None,
            metadata: None,
        };
        save_org(&orgs_dir, &org_with_repo("hypermemetic", vec![repo])).unwrap();
        save_org(&orgs_dir, &org_with_repo("hypermemetic-ai", vec![])).unwrap();

        // Local checkout with a .hyperforge/config.toml declaring the
        // source org.
        let checkout = config_dir.join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        let cfg = crate::v5::ops::fs::HyperforgeRepoConfig {
            repo_name: "widget".into(),
            org: OrgName::from("hypermemetic"),
            forges: vec![ProviderKind::Github],
            default_branch: None,
            visibility: None,
            description: None,
        };
        crate::v5::ops::fs::write_hyperforge_config(&checkout, &cfg, false).unwrap();

        // Run the migration.
        let service = ReposService::with_config_dir(config_dir.clone());
        let events: Vec<RepoEvent> = service
            .migrate_org(
                "hypermemetic".into(),
                "widget".into(),
                "hypermemetic-ai".into(),
                Some(checkout.display().to_string()),
                Some(serde_json::Value::Bool(false)),
            )
            .await
            .collect()
            .await;

        // Exactly one RepoMigrated, no errors.
        assert!(
            events.iter().any(|e| matches!(
                e,
                RepoEvent::RepoMigrated { old_org, new_org, .. }
                    if old_org == "hypermemetic" && new_org == "hypermemetic-ai"
            )),
            "expected RepoMigrated event, got {events:?}",
        );
        assert!(
            !events.iter().any(|e| matches!(e, RepoEvent::Error { .. })),
            "no error events expected, got {events:?}",
        );

        // Source org no longer lists the repo; target org now does.
        let reloaded = load_all(&config_dir).unwrap();
        let source = reloaded.orgs.get(&OrgName::from("hypermemetic")).unwrap();
        let target = reloaded.orgs.get(&OrgName::from("hypermemetic-ai")).unwrap();
        assert!(find_repo(source, "widget").is_none(), "repo must leave source org");
        let moved = find_repo(target, "widget").expect("repo must land in target org");

        // Remote URL org segment retargeted.
        assert_eq!(
            moved.remotes[0].url.as_str(),
            "git@github.com:hypermemetic-ai/widget.git",
            "remote URL org segment must be retargeted",
        );

        // Local config.toml org flipped.
        let flipped = crate::v5::ops::fs::read_hyperforge_config(&checkout)
            .unwrap()
            .expect("config.toml present");
        assert_eq!(flipped.org.as_str(), "hypermemetic-ai", "config.toml org must flip");
    }

    /// HYPE-6: the target org is CREATED when it isn't already registered
    /// (an owner-rename lands under the canonical org even if never
    /// bootstrapped). It inherits the source org's forge blocks.
    #[tokio::test]
    async fn test_migrate_org_creates_missing_target() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().to_path_buf();
        let orgs_dir = config_dir.join("orgs");
        std::fs::create_dir_all(&orgs_dir).unwrap();
        let repo = OrgRepo {
            name: RepoName::from("widget"),
            remotes: vec![ssh_remote("hypermemetic", "widget")],
            forges: None,
            metadata: None,
        };
        save_org(&orgs_dir, &org_with_repo("hypermemetic", vec![repo])).unwrap();

        let service = ReposService::with_config_dir(config_dir.clone());
        let events: Vec<RepoEvent> = service
            .migrate_org(
                "hypermemetic".into(),
                "widget".into(),
                "ghost-org".into(),
                None,
                Some(serde_json::Value::Bool(false)),
            )
            .await
            .collect()
            .await;
        assert!(
            !events.iter().any(|e| matches!(e, RepoEvent::Error { .. })),
            "no error expected — target org is auto-created, got {events:?}",
        );
        // Source lost the repo; the newly-created target org holds it.
        let reloaded = load_all(&config_dir).unwrap();
        let source = reloaded.orgs.get(&OrgName::from("hypermemetic")).unwrap();
        assert!(find_repo(source, "widget").is_none(), "repo must leave source org");
        let target = reloaded
            .orgs
            .get(&OrgName::from("ghost-org"))
            .expect("target org must be auto-created");
        assert!(find_repo(target, "widget").is_some(), "repo must land in created target org");
    }

    fn codeberg_remote(org: &str, repo: &str) -> Remote {
        Remote {
            url: RemoteUrl::from(format!("git@codeberg.org:{org}/{repo}.git").as_str()),
            provider: Some(ProviderKind::Codeberg),
        }
    }

    /// HYPE-6 (provider-scoped): a github owner migration retargets ONLY the
    /// github remote in the OrgRepo yaml; the codeberg URL is byte-identical.
    #[tokio::test]
    async fn test_migrate_one_provider_scoped_leaves_other_forges() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().to_path_buf();
        let orgs_dir = config_dir.join("orgs");
        std::fs::create_dir_all(&orgs_dir).unwrap();

        let repo = OrgRepo {
            name: RepoName::from("widget"),
            remotes: vec![
                ssh_remote("hypermemetic", "widget"),      // github, provider tagged
                codeberg_remote("hypermemetic", "widget"), // codeberg, provider tagged
            ],
            forges: None,
            metadata: None,
        };
        save_org(&orgs_dir, &org_with_repo("hypermemetic", vec![repo])).unwrap();

        let out = migrate_one(
            &config_dir,
            "hypermemetic",
            "widget",
            "hypermemetic-ai",
            ProviderKind::Github,
            None,
            false,
        )
        .expect("migrate ok");
        assert_eq!(out.old_full_name, "hypermemetic/widget");
        assert_eq!(out.new_full_name, "hypermemetic-ai/widget");

        let reloaded = load_all(&config_dir).unwrap();
        let target = reloaded.orgs.get(&OrgName::from("hypermemetic-ai")).unwrap();
        let moved = find_repo(target, "widget").expect("moved");
        let gh = moved
            .remotes
            .iter()
            .find(|r| r.url.as_str().contains("github.com"))
            .unwrap();
        let cb = moved
            .remotes
            .iter()
            .find(|r| r.url.as_str().contains("codeberg.org"))
            .unwrap();
        assert_eq!(
            gh.url.as_str(),
            "git@github.com:hypermemetic-ai/widget.git",
            "github remote retargeted to the new owner",
        );
        assert_eq!(
            cb.url.as_str(),
            "git@codeberg.org:hypermemetic/widget.git",
            "codeberg URL must be byte-identical (other forge untouched)",
        );
    }

    /// HYPE-6 (remote-names-from-config): migrate_one retargets the local
    /// remote by its ACTUAL .git/config name and only on the migrating
    /// provider — here Pattern A (origin=codeberg, github=github). The
    /// codeberg `origin` is left untouched; the `github` remote is retargeted.
    #[tokio::test]
    async fn test_migrate_one_targets_config_remote_names() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().to_path_buf();
        let orgs_dir = config_dir.join("orgs");
        std::fs::create_dir_all(&orgs_dir).unwrap();

        // provider_map lets derive_provider classify the raw local remotes.
        std::fs::write(
            config_dir.join("config.yaml"),
            "provider_map:\n  github.com: github\n  codeberg.org: codeberg\n",
        )
        .unwrap();

        let repo = OrgRepo {
            name: RepoName::from("widget"),
            remotes: vec![
                ssh_remote("hypermemetic", "widget"),
                codeberg_remote("hypermemetic", "widget"),
            ],
            forges: None,
            metadata: None,
        };
        save_org(&orgs_dir, &org_with_repo("hypermemetic", vec![repo])).unwrap();

        // Real checkout, Pattern A: origin=codeberg, github=github.
        let checkout = config_dir.join("widget");
        std::fs::create_dir_all(&checkout).unwrap();
        run_git(&checkout, &["init", "-q"]);
        run_git(&checkout, &["remote", "add", "origin", "git@codeberg.org:hypermemetic/widget.git"]);
        run_git(&checkout, &["remote", "add", "github", "git@github.com:hypermemetic/widget.git"]);
        let cfg = crate::v5::ops::fs::HyperforgeRepoConfig {
            repo_name: "widget".into(),
            org: OrgName::from("hypermemetic"),
            forges: vec![ProviderKind::Github],
            default_branch: None,
            visibility: None,
            description: None,
        };
        crate::v5::ops::fs::write_hyperforge_config(&checkout, &cfg, false).unwrap();

        migrate_one(
            &config_dir,
            "hypermemetic",
            "widget",
            "hypermemetic-ai",
            ProviderKind::Github,
            Some(&checkout),
            false,
        )
        .expect("migrate ok");

        let named: std::collections::BTreeMap<String, String> =
            read_named_remotes(&checkout).unwrap().into_iter().collect();
        assert_eq!(
            named.get("origin").map(String::as_str),
            Some("git@codeberg.org:hypermemetic/widget.git"),
            "codeberg `origin` must be untouched (not provider-convention retargeted)",
        );
        assert_eq!(
            named.get("github").map(String::as_str),
            Some("git@github.com:hypermemetic-ai/widget.git"),
            "the remote NAMED `github` in .git/config must be retargeted",
        );
    }

    /// HYPE-6 (doctor-detects): the pure verdict reports real divergence and
    /// treats owner-aliases as clean.
    #[test]
    fn test_doctor_verdict_detects_divergence_and_aliases_clean() {
        let mut owner_aliases = BTreeMap::new();
        owner_aliases.insert(
            OrgName::from("hypermemetic-ai"),
            vec![OrgName::from("hypermemetic")],
        );
        let g = GlobalConfig {
            owner_aliases,
            ..Default::default()
        };
        // aliased-only: registry under the org, forge answers the user alias.
        assert_eq!(
            doctor_verdict(&g, "hypermemetic-ai", "hypermemetic"),
            DoctorVerdict::Clean,
        );
        // genuine divergence: forge owner is an unrelated identity.
        assert_eq!(doctor_verdict(&g, "oldorg", "neworg"), DoctorVerdict::Diverged);
        // identical owner: clean.
        assert_eq!(doctor_verdict(&g, "acme", "acme"), DoctorVerdict::Clean);
    }

    /// HYPE-6 (heal-reuses-migrate): a seeded divergence heals through the
    /// SAME migrate_one — the registry entry ends under the canonical owner
    /// and the rename map records old→new full_name. (doctor_one calls
    /// exactly these two on divergence; the forge round-trip is
    /// integration-only, per the repo's no-mock convention.)
    #[tokio::test]
    async fn test_heal_reuses_migrate_and_records_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().to_path_buf();
        let orgs_dir = config_dir.join("orgs");
        std::fs::create_dir_all(&orgs_dir).unwrap();

        // Seeded divergence: repo registered under `oldorg`, forge canonical
        // owner is `neworg` (not aliases).
        let repo = OrgRepo {
            name: RepoName::from("widget"),
            remotes: vec![ssh_remote("oldorg", "widget")],
            forges: None,
            metadata: None,
        };
        save_org(&orgs_dir, &org_with_repo("oldorg", vec![repo])).unwrap();

        let out = migrate_one(
            &config_dir,
            "oldorg",
            "widget",
            "neworg",
            ProviderKind::Github,
            None,
            false,
        )
        .expect("heal migrate ok");
        assert_eq!(out.old_full_name, "oldorg/widget");
        assert_eq!(out.new_full_name, "neworg/widget");

        let reloaded = load_all(&config_dir).unwrap();
        assert!(
            find_repo(reloaded.orgs.get(&OrgName::from("oldorg")).unwrap(), "widget").is_none(),
            "repo leaves the diverged owner",
        );
        let target = reloaded
            .orgs
            .get(&OrgName::from("neworg"))
            .expect("canonical org auto-created");
        assert!(
            find_repo(target, "widget").is_some(),
            "registry entry ends under the canonical owner",
        );

        let path = write_rename_map(
            &config_dir,
            std::slice::from_ref(&(out.old_full_name.clone(), out.new_full_name.clone())),
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"oldorg/widget\""), "rename map has old full_name");
        assert!(raw.contains("\"neworg/widget\""), "rename map has new full_name");
    }

    /// HYPERFORGE-58: the doctor path end to end with the forge's answer
    /// injected: dry heal writes nothing; a real heal moves the registry
    /// entry AND flips the checkout's `origin`; unresolved never migrates.
    #[test]
    fn test_doctor_apply_heals_registry_and_origin_only_when_not_dry() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().to_path_buf();
        let orgs_dir = config_dir.join("orgs");
        std::fs::create_dir_all(&orgs_dir).unwrap();
        std::fs::write(config_dir.join("config.yaml"), "provider_map:\n  github.com: github\n").unwrap();
        let repo = OrgRepo {
            name: RepoName::from("widget"),
            remotes: vec![ssh_remote("oldorg", "widget")],
            forges: None,
            metadata: None,
        };
        save_org(&orgs_dir, &org_with_repo("oldorg", vec![repo])).unwrap();
        let checkout = tmp.path().join("widget");
        std::fs::create_dir_all(&checkout).unwrap();
        run_git(&checkout, &["init", "-q"]);
        run_git(&checkout, &["remote", "add", "origin", "git@github.com:oldorg/widget.git"]);
        let origin = || {
            crate::v5::ops::git::run_in(&checkout, &["remote", "get-url", "origin"])
                .unwrap()
                .trim()
                .to_string()
        };
        let global = GlobalConfig::default();
        let gh = Some(ProviderKind::Github);

        // Unresolved forge answer: unknown, no write.
        let (ev, rn) = doctor_apply(&config_dir, "oldorg", "widget", &global, Err("not_found".into()), gh, true, Some(&checkout), false);
        assert!(rn.is_none());
        assert!(ev.iter().any(|e| matches!(e, RepoEvent::RepoDoctorEntry { verdict, reason: Some(r), .. } if verdict == "unknown" && r == "not_found")));

        // Report only (no heal): diverged, no write.
        let (ev, rn) = doctor_apply(&config_dir, "oldorg", "widget", &global, Ok("neworg".into()), gh, false, Some(&checkout), false);
        assert!(rn.is_none());
        assert!(ev.iter().any(|e| matches!(e, RepoEvent::RepoDoctorEntry { verdict, .. } if verdict == "diverged")));

        // Dry heal: reported, nothing written.
        let (ev, _) = doctor_apply(&config_dir, "oldorg", "widget", &global, Ok("neworg".into()), gh, true, Some(&checkout), true);
        assert!(ev.iter().any(|e| matches!(e, RepoEvent::RepoHealed { dry_run: true, .. })), "{ev:?}");
        let reloaded = load_all(&config_dir).unwrap();
        assert!(find_repo(reloaded.orgs.get(&OrgName::from("oldorg")).unwrap(), "widget").is_some());
        assert_eq!(origin(), "git@github.com:oldorg/widget.git");

        // Real heal: registry and origin both move.
        let (ev, rn) = doctor_apply(&config_dir, "oldorg", "widget", &global, Ok("neworg".into()), gh, true, Some(&checkout), false);
        assert!(ev.iter().any(|e| matches!(e, RepoEvent::RepoHealed { dry_run: false, .. })), "{ev:?}");
        assert_eq!(rn, Some(("oldorg/widget".into(), "neworg/widget".into())));
        let reloaded = load_all(&config_dir).unwrap();
        assert!(find_repo(reloaded.orgs.get(&OrgName::from("oldorg")).unwrap(), "widget").is_none());
        assert!(find_repo(reloaded.orgs.get(&OrgName::from("neworg")).unwrap(), "widget").is_some());
        assert_eq!(origin(), "git@github.com:neworg/widget.git");
    }

    /// HYPERFORGE-58: healing into an org that already has the repo drops
    /// the stale entry and leaves exactly one; owner case is not divergence.
    #[test]
    fn test_heal_into_existing_target_deduplicates_and_ignores_case() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().to_path_buf();
        let orgs_dir = config_dir.join("orgs");
        std::fs::create_dir_all(&orgs_dir).unwrap();
        std::fs::write(config_dir.join("config.yaml"), "provider_map:\n  github.com: github\n").unwrap();
        let mk = |org: &str| OrgRepo {
            name: RepoName::from("widget"),
            remotes: vec![ssh_remote(org, "widget")],
            forges: None,
            metadata: None,
        };
        save_org(&orgs_dir, &org_with_repo("oldorg", vec![mk("oldorg")])).unwrap();
        save_org(&orgs_dir, &org_with_repo("neworg", vec![mk("neworg")])).unwrap();
        let global = GlobalConfig::default();
        assert_eq!(doctor_verdict(&global, "juggernautlabs", "JuggernautLabs"), DoctorVerdict::Clean);

        let (ev, _) = doctor_apply(&config_dir, "oldorg", "widget", &global, Ok("neworg".into()), Some(ProviderKind::Github), true, None, false);
        assert!(ev.iter().any(|e| matches!(e, RepoEvent::RepoHealed { already_in_target: true, dry_run: false, .. })), "{ev:?}");
        let reloaded = load_all(&config_dir).unwrap();
        assert!(find_repo(reloaded.orgs.get(&OrgName::from("oldorg")).unwrap(), "widget").is_none());
        let target = reloaded.orgs.get(&OrgName::from("neworg")).unwrap();
        assert_eq!(target.repos.iter().filter(|r| r.name.as_str() == "widget").count(), 1);
    }

    /// `dry_run` performs no writes.
    #[tokio::test]
    async fn test_migrate_org_dry_run_no_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().to_path_buf();
        let orgs_dir = config_dir.join("orgs");
        std::fs::create_dir_all(&orgs_dir).unwrap();
        let repo = OrgRepo {
            name: RepoName::from("widget"),
            remotes: vec![ssh_remote("hypermemetic", "widget")],
            forges: None,
            metadata: None,
        };
        save_org(&orgs_dir, &org_with_repo("hypermemetic", vec![repo])).unwrap();
        save_org(&orgs_dir, &org_with_repo("hypermemetic-ai", vec![])).unwrap();

        let service = ReposService::with_config_dir(config_dir.clone());
        let events: Vec<RepoEvent> = service
            .migrate_org(
                "hypermemetic".into(),
                "widget".into(),
                "hypermemetic-ai".into(),
                None,
                Some(serde_json::Value::Bool(true)),
            )
            .await
            .collect()
            .await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                RepoEvent::RepoMigrated { dry_run: true, .. }
            )),
            "expected dry-run RepoMigrated, got {events:?}",
        );
        // Membership unchanged on disk.
        let reloaded = load_all(&config_dir).unwrap();
        let source = reloaded.orgs.get(&OrgName::from("hypermemetic")).unwrap();
        let target = reloaded.orgs.get(&OrgName::from("hypermemetic-ai")).unwrap();
        assert!(find_repo(source, "widget").is_some(), "dry_run must not move repo");
        assert!(find_repo(target, "widget").is_none(), "dry_run must not write target");
    }

    #[tokio::test]
    async fn test_migrate_org_defaults_to_dry_run() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().to_path_buf();
        let orgs_dir = config_dir.join("orgs");
        std::fs::create_dir_all(&orgs_dir).unwrap();
        let repo = OrgRepo {
            name: RepoName::from("widget"),
            remotes: vec![ssh_remote("hypermemetic", "widget")],
            forges: None,
            metadata: None,
        };
        save_org(&orgs_dir, &org_with_repo("hypermemetic", vec![repo])).unwrap();
        save_org(&orgs_dir, &org_with_repo("hypermemetic-ai", vec![])).unwrap();

        let service = ReposService::with_config_dir(config_dir.clone());
        let events: Vec<RepoEvent> = service
            .migrate_org(
                "hypermemetic".into(),
                "widget".into(),
                "hypermemetic-ai".into(),
                None,
                None,
            )
            .await
            .collect()
            .await;
        assert!(
            events.iter().any(|e| matches!(
                e,
                RepoEvent::RepoMigrated { dry_run: true, .. }
            )),
            "expected dry-run RepoMigrated, got {events:?}",
        );
        // Membership unchanged on disk.
        let reloaded = load_all(&config_dir).unwrap();
        let source = reloaded.orgs.get(&OrgName::from("hypermemetic")).unwrap();
        let target = reloaded.orgs.get(&OrgName::from("hypermemetic-ai")).unwrap();
        assert!(find_repo(source, "widget").is_some(), "dry_run must not move repo");
        assert!(find_repo(target, "widget").is_none(), "dry_run must not write target");
    }
}

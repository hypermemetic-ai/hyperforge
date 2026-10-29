//! `HyperforgeService` - Root activation for hyperforge
//!
//! This is a service plugin that routes to child sub-services:
//! - repo: Single-repo operations and registry CRUD
//! - workspace: Multi-repo workspace orchestration

use async_stream::stream;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use chrono::Utc;
use std::collections::HashMap;

use crate::adapters::{ForgePort, ForgeSyncState};
use crate::auth::credentials::{
    credentials_for_channels, credentials_for_forge, CredentialKind, CredentialSpec,
    ResolvedCredential, ValidationMethod,
};
use crate::auth::AuthProvider;
use crate::auth::YamlAuthProvider;
use crate::auth_service::storage::YamlStorage;
use crate::auth_service::types::SecretPath;
use crate::commands::runner::discover_or_bail;
use crate::config::{HyperforgeConfig, OrgConfig};
use crate::services::utils::{make_adapter, RepoFilter};
use crate::services::{BuildService, HyperforgeState, RepoService, WorkspaceService};
use crate::types::config::DistChannel;
use crate::types::repo::RepoRecord;
use crate::types::Forge;

/// Package registry identifier
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PackageRegistry {
    CratesIo,
    Hackage,
    Npm,
}

impl std::fmt::Display for PackageRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CratesIo => write!(f, "crates.io"),
            Self::Hackage => write!(f, "hackage"),
            Self::Npm => write!(f, "npm"),
        }
    }
}

/// Package status relative to its registry
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PackageStatus {
    /// Local version > published version
    Ahead,
    /// Local version == published version, no changes needed
    UpToDate,
    /// Never published to registry
    Unpublished,
    /// Local version == published version but needs bump (code changed)
    Stale,
    /// Version matches but code changed since publish tag
    Drifted,
}

/// Action taken or planned during publish
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublishActionKind {
    /// Local > published, publish as-is
    Publish,
    /// Local == published, bump patch then publish
    AutoBump,
    /// First publish ever
    InitialPublish,
    /// Already up to date, skip
    Skip,
    /// Git tag created
    Tag,
    /// Publish failed
    Failed,
}

/// Machine-readable discriminator for why `orgs_add` refused a request.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrgAddFailureReason {
    /// The `org` value was empty or contained a path separator / null byte.
    InvalidName,
    /// An org config already exists for this name.
    AlreadyExists,
    /// An I/O or serialization error occurred while writing the config.
    IoError,
}

/// Machine-readable discriminator for why `repo set_transport` refused a request.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransportChangeFailureReason {
    /// The target path is not a hyperforge-registered repo (no `.hyperforge/config.toml`).
    NotInitialized,
    /// The target path exists but is not a git repo.
    NotAGitRepo,
    /// The target path does not exist or is not readable.
    PathNotFound,
    /// A git command (list remotes, set-url) failed.
    GitError,
    /// The `.hyperforge/config.toml` could not be loaded.
    ConfigError,
}

/// Machine-readable discriminator for why `orgs_update` refused a request.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrgUpdateFailureReason {
    /// No org config exists for the given name.
    NotFound,
    /// The caller supplied no mutation fields (neither ssh nor workspace_path).
    NoFieldsToUpdate,
    /// An I/O or serialization error occurred while writing the config.
    IoError,
}

/// Machine-readable description of what `orgs_update` did (or would do, in
/// dry-run mode). A single call can produce multiple operations — e.g. an
/// ssh merge plus a workspace_path clear.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OrgUpdateOp {
    /// Per-field merge: ssh fields the caller provided were upserted;
    /// fields the caller omitted were left alone.
    SshMerged,
    /// Whole-record replacement: the persisted ssh state now equals
    /// exactly what the caller provided.
    SshReplaced,
    /// `workspace_path` set to a specific value.
    WorkspacePathSet,
    /// `workspace_path` cleared (absent in the stored config afterwards).
    WorkspacePathCleared,
}

/// Hyperforge event types
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HyperforgeEvent {
    /// Status information
    Status {
        version: String,
        description: String,
    },
    /// General info message
    Info { message: String },
    /// Error message
    Error { message: String },
    /// Repository information
    Repo {
        name: String,
        description: Option<String>,
        visibility: String,
        origin: String,
        mirrors: Vec<String>,
        protected: bool,
        #[serde(default, skip_serializing_if = "crate::types::repo::is_false")]
        staged_for_deletion: bool,
    },
    /// Sync diff result - repo operation
    SyncOp {
        repo_name: String,
        operation: String, // "create", "update", "delete", "in_sync"
        forge: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        details: Vec<String>,
    },
    /// Sync summary
    SyncSummary {
        forge: String,
        total: usize,
        to_create: usize,
        to_update: usize,
        to_delete: usize,
        in_sync: usize,
    },
    /// Per-repo check result (branch + clean status)
    RepoCheck {
        repo_name: String,
        path: String,
        branch: String,
        expected_branch: String,
        is_clean: bool,
        on_correct_branch: bool,
    },
    /// Per-repo push result
    RepoPush {
        repo_name: String,
        path: String,
        forge: String,
        success: bool,
        error: Option<String>,
    },
    /// Workspace-level summary
    WorkspaceSummary {
        total_repos: usize,
        configured_repos: usize,
        unconfigured_repos: usize,
        clean_repos: Option<usize>,
        dirty_repos: Option<usize>,
        wrong_branch_repos: Option<usize>,
        push_success: Option<usize>,
        push_failed: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        validation_passed: Option<bool>,
    },
    /// Result of workspace unify (native workspace file generation)
    UnifyResult {
        language: String,
        file_path: String,
        action: String, // "created", "updated", "unchanged"
    },
    /// Dependency version mismatch between pinned and local
    DepMismatch {
        repo: String,
        dependency: String,
        pinned_version: String,
        local_version: String,
    },
    /// Validation step result
    ValidateStep {
        repo_name: String,
        step: String, // "build" or "test"
        status: String, // "passed", "failed", "skipped"
        duration_ms: u64,
    },
    /// Validation summary
    ValidateSummary {
        total: usize,
        passed: usize,
        failed: usize,
        skipped: usize,
        duration_ms: u64,
    },
    /// Per-repo move step result
    RepoMove {
        repo_name: String,
        step: String,      // "config", "remotes", "registry", "directory"
        success: bool,
        message: String,
    },
    /// Result of workspace exec command for a single repo
    ExecResult {
        repo_name: String,
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    /// Package registry diff — local vs published version
    PackageDiff {
        package_name: String,
        build_system: crate::build_system::BuildSystemKind,
        local_version: String,
        published_version: Option<String>,
        registry: PackageRegistry,
        status: PackageStatus,
        /// Files that differ between local and published artifact.
        #[serde(skip_serializing_if = "Option::is_none")]
        changed_files: Option<Vec<String>>,
    },
    /// Per-package publish step result
    PublishStep {
        package_name: String,
        version: String,
        registry: PackageRegistry,
        action: PublishActionKind,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Summary of a publish run
    PublishSummary {
        total: usize,
        published: usize,
        auto_bumped: usize,
        skipped: usize,
        failed: usize,
        tags_created: usize,
    },
    /// Large tracked file detected in a repository
    LargeFile {
        repo_name: String,
        file_path: String,
        size_bytes: u64,
        /// True if the file is only in git history (deleted from working tree)
        #[serde(default, skip_serializing_if = "crate::types::repo::is_false")]
        history_only: bool,
    },
    /// Repository size summary
    RepoSize {
        repo_name: String,
        tracked_files: usize,
        total_bytes: u64,
    },
    /// Lines-of-code count for a repository
    RepoLoc {
        repo_name: String,
        total_lines: usize,
        total_files: usize,
        by_extension: std::collections::HashMap<String, usize>,
    },
    /// Repository dirty status
    RepoDirty {
        repo_name: String,
        has_staged: bool,
        has_changes: bool,
        has_untracked: bool,
        branch: String,
    },
    /// Container image tag
    ImageTag {
        repo_name: String,
        forge: String,
        tag: String,
        digest: String,
        size_bytes: u64,
        created_at: String,
    },
    /// Container image push result
    ImagePush {
        repo_name: String,
        forge: String,
        tag: String,
        image: String,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Container image delete result
    ImageDelete {
        repo_name: String,
        forge: String,
        tag: String,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Release information
    ReleaseInfo {
        repo_name: String,
        forge: String,
        tag: String,
        title: String,
        asset_count: usize,
        draft: bool,
        prerelease: bool,
        created_at: String,
    },
    /// Release creation result
    ReleaseCreate {
        repo_name: String,
        forge: String,
        tag: String,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Release asset upload result
    ReleaseUpload {
        repo_name: String,
        forge: String,
        tag: String,
        asset_name: String,
        size_bytes: u64,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Release deletion result
    ReleaseDelete {
        repo_name: String,
        forge: String,
        tag: String,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Release asset information
    AssetInfo {
        repo_name: String,
        forge: String,
        tag: String,
        asset_name: String,
        size_bytes: u64,
        content_type: String,
        download_url: String,
        created_at: String,
    },
    /// Auth credential check result
    AuthCheckResult {
        credential: String,
        key_path: String,
        status: String, // "valid", "missing", "invalid", "insufficient_scopes"
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Result of `orgs_add` — an organization was created (or would have been, in dry-run mode).
    OrgAdded {
        org: String,
        /// True when this was a preview (`dry_run: true`); no file was written.
        dry_run: bool,
    },
    /// `orgs_add` refused the request. `reason` is a machine-readable
    /// discriminator: callers can match on it without parsing `message`.
    OrgAddFailed {
        org: String,
        reason: OrgAddFailureReason,
        message: String,
    },
    /// Result of `orgs_update` — an org config was mutated (or would have
    /// been, in dry-run mode). `operations` enumerates every distinct
    /// action performed, so callers can verify merge vs replace vs
    /// workspace_path changes from the event stream alone.
    OrgUpdated {
        org: String,
        dry_run: bool,
        operations: Vec<OrgUpdateOp>,
    },
    /// `orgs_update` refused the request. `reason` is a machine-readable
    /// discriminator.
    OrgUpdateFailed {
        org: String,
        reason: OrgUpdateFailureReason,
        message: String,
    },
    /// Reports the current transport of a configured forge remote, read
    /// live from `git remote`. Emitted by `repo status` (once per forge)
    /// and by `repo set_transport` after a change. The value reflects
    /// actual on-disk state, not a cached field — this ensures repos
    /// initialized before the TRANSPORT epic still report correctly.
    RepoTransport {
        /// Repository path the caller passed in.
        path: String,
        /// Git remote name, e.g. `origin` or `codeberg`.
        remote: String,
        /// Forge string, e.g. `github`.
        forge: String,
        /// Detected transport. `ssh` or `https`. `None` when the remote's
        /// URL shape is not recognized (e.g. a custom mirror URL).
        transport: Option<crate::git::Transport>,
        /// The remote URL as stored in `git remote`, for debugging.
        url: Option<String>,
    },
    /// `repo set_transport` rewrote the URL of one or more remotes.
    TransportChanged {
        /// Repository path the caller passed in.
        path: String,
        /// Requested transport.
        transport: crate::git::Transport,
        /// Remote names that were updated (e.g. `["origin", "codeberg"]`).
        remotes_changed: Vec<String>,
    },
    /// `repo set_transport` was a no-op: every configured forge's remote
    /// already matched the requested transport. No `git remote` command
    /// was invoked.
    TransportUnchanged {
        path: String,
        transport: crate::git::Transport,
    },
    /// `repo set_transport` refused the request. `reason` is a
    /// machine-readable discriminator.
    TransportChangeFailed {
        path: String,
        reason: TransportChangeFailureReason,
        message: String,
    },
    /// Progress step during build release orchestration
    ReleaseBuildStep {
        repo_name: String,
        target: String,
        status: String, // "compiling", "packaging", "uploading"
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Summary of a build release run
    ReleaseSummary {
        repos: usize,
        targets: usize,
        forges: usize,
        assets_uploaded: usize,
        failed: usize,
    },
}

/// Validate a single credential token against its spec's validation method.
/// Returns (status, detail).
async fn validate_credential(spec: &CredentialSpec, token: &str) -> (String, Option<String>) {
    match &spec.validation {
        ValidationMethod::ExistsOnly => ("valid".to_string(), None),
        ValidationMethod::HttpGet {
            url_pattern,
            auth_scheme,
        } => {
            let client = reqwest::Client::new();
            let res = client
                .get(*url_pattern)
                .header("Authorization", format!("{auth_scheme} {token}"))
                .header("User-Agent", "hyperforge")
                .send()
                .await;
            match res {
                Ok(resp) if resp.status().is_success() => ("valid".to_string(), None),
                Ok(resp) => {
                    let code = resp.status().as_u16();
                    (
                        "invalid".to_string(),
                        Some(format!("HTTP {code} from {url_pattern}")),
                    )
                }
                Err(e) => (
                    "invalid".to_string(),
                    Some(format!("Request failed: {e}")),
                ),
            }
        }
        ValidationMethod::GitHubScopes { required } => {
            let client = reqwest::Client::new();
            let res = client
                .get("https://api.github.com/user")
                .header("Authorization", format!("Bearer {token}"))
                .header("User-Agent", "hyperforge")
                .send()
                .await;
            match res {
                Ok(resp) if resp.status().is_success() => {
                    let scopes_header = resp
                        .headers()
                        .get("x-oauth-scopes")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let have: Vec<&str> = scopes_header
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .collect();
                    let missing: Vec<&&str> = required
                        .iter()
                        .filter(|r| !have.iter().any(|h| *h == **r))
                        .collect();
                    if missing.is_empty() {
                        (
                            "valid".to_string(),
                            Some(format!("scopes: {}", have.join(", "))),
                        )
                    } else {
                        (
                            "insufficient_scopes".to_string(),
                            Some(format!(
                                "missing: {}; have: {}",
                                missing.iter().map(|s| **s).collect::<Vec<_>>().join(", "),
                                have.join(", ")
                            )),
                        )
                    }
                }
                Ok(resp) => {
                    let code = resp.status().as_u16();
                    (
                        "invalid".to_string(),
                        Some(format!("HTTP {code} from GitHub /user")),
                    )
                }
                Err(e) => (
                    "invalid".to_string(),
                    Some(format!("Request failed: {e}")),
                ),
            }
        }
    }
}

/// Root service for hyperforge operations
#[derive(Clone)]
pub struct HyperforgeService {
    pub(crate) state: HyperforgeState,
}

impl HyperforgeService {
    /// Create a new `HyperforgeService` instance
    pub fn new() -> Self {
        Self {
            state: HyperforgeState::new(),
        }
    }

    /// Create a HyperforgeService rooted at an arbitrary config directory.
    ///
    /// Tests use this to isolate disk I/O under a tempdir.
    pub fn new_with_config_dir(config_dir: std::path::PathBuf) -> Self {
        Self {
            state: HyperforgeState::new_with_config_dir(config_dir),
        }
    }
}

impl Default for HyperforgeService {
    fn default() -> Self {
        Self::new()
    }
}

#[plexus_macros::activation(
    namespace = "hyperforge",
    description = "Multi-forge repository management",
    crate_path = "plexus_core"
)]
impl HyperforgeService {
    /// Single-repo operations and registry CRUD.
    #[plexus_macros::child]
    fn repo(&self) -> RepoService {
        RepoService::new(self.state.clone())
    }

    /// Multi-repo workspace orchestration.
    #[plexus_macros::child]
    fn workspace(&self) -> WorkspaceService {
        WorkspaceService::new(self.state.clone())
    }

    /// Build system orchestration (cargo, cabal, node, packaging).
    ///
    /// `&self` is required by `#[plexus_macros::child]` (the macro
    /// generates a `ChildRouter` accessor that takes `&Self`). `BuildService`
    /// carries no state of its own, so the receiver is unused here.
    #[plexus_macros::child]
    #[allow(clippy::unused_self)]
    const fn build(&self) -> BuildService {
        BuildService::new()
    }

    /// Show hyperforge status
    #[plexus_macros::method(description = "Show hyperforge status and version")]
    pub async fn status(&self) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        stream! {
            yield HyperforgeEvent::Status {
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: "Multi-forge repository management (FORGE4: state mirror + SSH safety)".to_string(),
            };
        }
    }

    /// Reload cached state from disk (repos.yaml for all known orgs)
    #[plexus_macros::method(description = "Reload all cached LocalForge state from disk")]
    pub async fn reload(&self) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();
        stream! {
            let reloaded = state.reload().await;
            if reloaded.is_empty() {
                yield HyperforgeEvent::Info {
                    message: "No cached orgs to reload. Run a command first to populate the cache.".to_string(),
                };
            } else {
                yield HyperforgeEvent::Info {
                    message: format!("Reloaded {} org(s): {}", reloaded.len(), reloaded.join(", ")),
                };
            }
        }
    }

    /// Bootstrap an org — import all repos from remote forges into `LocalForge`
    #[plexus_macros::method(
        description = "Bootstrap an org — import all repos from remote forges into LocalForge, creating the canonical state mirror. Can generate SSH keys and set a workspace path.",
        params(
            org = "Organization name",
            forges = "Forges to import from (e.g. github,codeberg)",
            dry_run = "Preview without writing state (optional, default: false)",
            generate_ssh_key = "Generate ed25519 SSH keys for each forge (optional, default: false)",
            workspace_path = "Workspace directory path for this org's repos (optional)"
        )
    )]
    pub async fn begin(
        &self,
        org: String,
        forges: Vec<String>,
        dry_run: Option<bool>,
        generate_ssh_key: Option<bool>,
        workspace_path: Option<String>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();
        let is_dry_run = dry_run.unwrap_or(false);
        let do_keygen = generate_ssh_key.unwrap_or(false);
        let config_dir = self.state.config_dir.clone();

        stream! {
            let dry_prefix = if is_dry_run { "[dry-run] " } else { "" };

            // Parse and validate forge strings
            let mut parsed_forges: Vec<(String, Forge)> = Vec::new();
            for forge_str in &forges {
                for part in forge_str.split(',') {
                    let part = part.trim();
                    if part.is_empty() {
                        continue;
                    }
                    if let Some(forge) = HyperforgeConfig::parse_forge(part) { parsed_forges.push((part.to_lowercase().clone(), forge)) } else {
                        yield HyperforgeEvent::Error {
                            message: format!("Invalid forge: {part}. Must be github, codeberg, or gitlab"),
                        };
                        return;
                    }
                }
            }

            if parsed_forges.is_empty() {
                yield HyperforgeEvent::Error {
                    message: "No forges specified. Provide at least one forge (github, codeberg, gitlab).".to_string(),
                };
                return;
            }

            yield HyperforgeEvent::Info {
                message: format!(
                    "{}Begin: bootstrapping org '{}' from {} forge(s): {}",
                    dry_prefix,
                    org,
                    parsed_forges.len(),
                    parsed_forges.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>().join(", "),
                ),
            };

            // Phase: SSH keygen + workspace path
            if do_keygen || workspace_path.is_some() {
                let mut org_config = OrgConfig::load(&config_dir, &org);

                if do_keygen {
                    for (forge_str, _) in &parsed_forges {
                        let key_path = OrgConfig::ssh_key_path(&config_dir, &org, forge_str);
                        if key_path.exists() {
                            yield HyperforgeEvent::Info {
                                message: format!("  Using existing SSH key for {}: {}", forge_str, key_path.display()),
                            };
                        } else if is_dry_run {
                            yield HyperforgeEvent::Info {
                                message: format!("  {}Would generate SSH key for {}: {}", dry_prefix, forge_str, key_path.display()),
                            };
                        } else {
                            match OrgConfig::generate_ssh_key(&config_dir, &org, forge_str) {
                                Ok(path) => {
                                    yield HyperforgeEvent::Info {
                                        message: format!("  Generated SSH key for {}: {}", forge_str, path.display()),
                                    };
                                    yield HyperforgeEvent::Info {
                                        message: format!("  Public key: {}.pub", path.display()),
                                    };
                                }
                                Err(e) => {
                                    yield HyperforgeEvent::Error {
                                        message: format!("  Failed to generate SSH key for {forge_str}: {e}"),
                                    };
                                    return;
                                }
                            }
                        }

                        if !is_dry_run {
                            org_config.ssh.insert(forge_str.clone(), key_path.to_string_lossy().to_string());
                        }
                    }
                }

                if let Some(ref wp) = workspace_path {
                    yield HyperforgeEvent::Info {
                        message: format!("  {dry_prefix}Workspace path: {wp}"),
                    };
                    if !is_dry_run {
                        org_config.workspace_path = Some(wp.clone());
                    }
                }

                if !is_dry_run {
                    if let Err(e) = org_config.save(&config_dir, &org) {
                        yield HyperforgeEvent::Error {
                            message: format!("Failed to save org config: {e}"),
                        };
                        return;
                    }
                }
            }

            // Phase 1: Verify auth by creating adapters
            let mut adapters: Vec<(String, Forge, Arc<dyn ForgePort>)> = Vec::new();

            let ot = state.get_local_forge(&org).await.owner_type();
            for (forge_str, forge_enum) in &parsed_forges {
                match make_adapter(forge_str, &org, ot.clone()) {
                    Ok(adapter) => {
                        yield HyperforgeEvent::Info {
                            message: format!("  Authenticated with {forge_str}"),
                        };
                        adapters.push((forge_str.clone(), forge_enum.clone(), adapter));
                    }
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("  Failed to authenticate with {forge_str}: {e}"),
                        };
                        return;
                    }
                }
            }

            // Phase 2: Import repos from each forge
            let local = state.get_local_forge(&org).await;
            let mut per_forge_counts: HashMap<String, usize> = HashMap::new();
            let mut total_upserted = 0usize;

            for (forge_str, forge_enum, adapter) in &adapters {
                yield HyperforgeEvent::Info {
                    message: format!("  {dry_prefix}Importing repos from {forge_str}..."),
                };

                let list_result = match adapter.list_repos_incremental(&org, None).await {
                    Ok(lr) => lr,
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("  Failed to list repos from {forge_str}: {e}"),
                        };
                        continue;
                    }
                };

                let mut forge_count = 0usize;

                if list_result.modified {
                    if let Some(repos) = &list_result.repos {
                        for repo in repos {
                            let record = RepoRecord::from_repo(repo);

                            if !is_dry_run {
                                if let Err(e) = local.upsert_record(record) {
                                    yield HyperforgeEvent::Error {
                                        message: format!("  Failed to upsert {}: {}", repo.name, e),
                                    };
                                    continue;
                                }
                            }

                            forge_count += 1;
                            total_upserted += 1;
                        }

                        yield HyperforgeEvent::Info {
                            message: format!("  {dry_prefix}Found {forge_count} repos on {forge_str}"),
                        };
                    }
                } else {
                    yield HyperforgeEvent::Info {
                        message: format!("  {forge_str} returned not-modified (no repos to import)"),
                    };
                }

                per_forge_counts.insert(forge_str.clone(), forge_count);

                // Store ETags
                if !is_dry_run {
                    if let Err(e) = local.set_forge_state(forge_enum.clone(), ForgeSyncState {
                        last_synced: Utc::now(),
                        etag: list_result.etag.clone(),
                    }) {
                        yield HyperforgeEvent::Error {
                            message: format!("  Failed to store sync state for {forge_str}: {e}"),
                        };
                    }
                }
            }

            // Save LocalForge to disk
            if !is_dry_run && total_upserted > 0 {
                if let Err(e) = local.save_to_yaml().await {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to save LocalForge for {org}: {e}"),
                    };
                }
            }

            // Summary
            let unique_count = match local.all_records() {
                Ok(records) => records.len(),
                Err(_) => total_upserted,
            };

            yield HyperforgeEvent::Info {
                message: format!(
                    "{dry_prefix}Begin complete: {unique_count} unique repos in LocalForge for '{org}'",
                ),
            };

            for (forge_str, count) in &per_forge_counts {
                yield HyperforgeEvent::Info {
                    message: format!("  {forge_str}: {count} repos imported"),
                };
            }

            if is_dry_run {
                yield HyperforgeEvent::Info {
                    message: "Dry run — no changes written to disk.".to_string(),
                };
            }
        }
    }

    /// Show org-level configuration (SSH keys, defaults)
    #[plexus_macros::method(
        description = "Show org-level configuration including SSH key defaults",
        params(
            org = "Organization name"
        )
    )]
    pub async fn config_show(
        &self,
        org: String,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        stream! {
            let org_config = OrgConfig::load(&config_dir, &org);
            let config_path = OrgConfig::config_path(&config_dir, &org);

            if org_config.ssh.is_empty() {
                yield HyperforgeEvent::Info {
                    message: format!(
                        "No org config for '{}'. Expected at: {}",
                        org,
                        config_path.display()
                    ),
                };
                yield HyperforgeEvent::Info {
                    message: "Set SSH keys with: synapse lforge hyperforge config_set_ssh_key --org <org> --forge <forge> --key <path>".to_string(),
                };
            } else {
                yield HyperforgeEvent::Info {
                    message: format!("Org config for '{}' ({})", org, config_path.display()),
                };
                yield HyperforgeEvent::Info {
                    message: "SSH keys:".to_string(),
                };
                for (forge, key_path) in &org_config.ssh {
                    yield HyperforgeEvent::Info {
                        message: format!("  {forge}: {key_path}"),
                    };
                }
            }
        }
    }

    /// Set an org-level default SSH key for a forge
    #[plexus_macros::method(
        description = "Set or update an org-level default SSH key for a forge",
        params(
            org = "Organization name",
            forge = "Forge name (github, codeberg, gitlab)",
            key = "Path to SSH private key (e.g. ~/.ssh/id_ed25519)"
        )
    )]
    pub async fn config_set_ssh_key(
        &self,
        org: String,
        forge: String,
        key: String,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        stream! {
            // Validate forge name
            if HyperforgeConfig::parse_forge(&forge).is_none() {
                yield HyperforgeEvent::Error {
                    message: format!("Invalid forge: {forge}. Must be github, codeberg, or gitlab"),
                };
                return;
            }

            // Expand ~ in key path for validation
            let expanded = if key.starts_with("~/") {
                dirs::home_dir().map_or_else(|| std::path::PathBuf::from(&key), |h| h.join(&key[2..]))
            } else {
                std::path::PathBuf::from(&key)
            };

            if !expanded.exists() {
                yield HyperforgeEvent::Error {
                    message: format!("SSH key not found: {} (expanded: {})", key, expanded.display()),
                };
                return;
            }

            let mut org_config = OrgConfig::load(&config_dir, &org);
            org_config.ssh.insert(forge.clone(), key.clone());

            match org_config.save(&config_dir, &org) {
                Ok(()) => {
                    let config_path = OrgConfig::config_path(&config_dir, &org);
                    yield HyperforgeEvent::Info {
                        message: format!("Set SSH key for {forge} on org '{org}': {key}"),
                    };
                    yield HyperforgeEvent::Info {
                        message: format!("Saved to {}", config_path.display()),
                    };
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to save org config: {e}"),
                    };
                }
            }
        }
    }

    /// Show the public SSH key for an org/forge (pipe to pbcopy)
    #[plexus_macros::method(
        description = "Show the public SSH key for an org/forge — pipe output to pbcopy",
        params(
            org = "Organization name",
            forge = "Forge name (github, codeberg, gitlab)"
        )
    )]
    pub async fn config_show_ssh_key(
        &self,
        org: String,
        forge: String,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        stream! {
            if HyperforgeConfig::parse_forge(&forge).is_none() {
                yield HyperforgeEvent::Error {
                    message: format!("Invalid forge: {forge}. Must be github, codeberg, or gitlab"),
                };
                return;
            }

            let org_config = OrgConfig::load(&config_dir, &org);
            if org_config.ssh_key_for_forge(&forge).is_none() {
                yield HyperforgeEvent::Error {
                    message: format!("No SSH key configured for {forge} on org '{org}'. Run begin with --generate_ssh_key true first."),
                };
                return;
            }

            match OrgConfig::read_public_key(&config_dir, &org, &forge) {
                Ok(pubkey) => {
                    yield HyperforgeEvent::Info { message: pubkey };
                }
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                }
            }
        }
    }

    /// List all known organizations
    #[plexus_macros::method(description = "List all organizations configured in hyperforge")]
    pub async fn orgs_list(&self) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        stream! {
            let orgs_dir = config_dir.join("orgs");
            if !orgs_dir.exists() {
                yield HyperforgeEvent::Info {
                    message: "No organizations configured.".to_string(),
                };
                return;
            }

            let mut orgs: Vec<String> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&orgs_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "toml") {
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            orgs.push(stem.to_string());
                        }
                    }
                }
            }

            orgs.sort();

            if orgs.is_empty() {
                yield HyperforgeEvent::Info {
                    message: "No organizations configured.".to_string(),
                };
                return;
            }

            for org in &orgs {
                let org_config = OrgConfig::load(&config_dir, org);
                let forges: Vec<&str> = org_config.ssh.keys().map(std::string::String::as_str).collect();
                let wp = org_config.workspace_path.as_deref().unwrap_or("(not set)");
                yield HyperforgeEvent::Info {
                    message: format!(
                        "  {} — workspace: {}, forges: [{}]",
                        org,
                        wp,
                        forges.join(", "),
                    ),
                };
            }

            yield HyperforgeEvent::Info {
                message: format!("{} org(s) configured.", orgs.len()),
            };
        }
    }

    /// Add a new organization — creates its TOML config from RPC parameters
    #[plexus_macros::method(
        description = "Create a new org config from RPC parameters. Validates the org name, rejects if an org with this name already exists, and supports dry-run. Composes with `orgs_delete` + `orgs_add` for renames.",
        params(
            org = "Organization name (must not be empty or contain path separators)",
            ssh = "SSH key paths per forge (optional). One field per known forge — Synapse emits per-forge flags like --ssh.github.",
            workspace_path = "Workspace directory path for this org's repos (optional)",
            dry_run = "Preview without writing state (optional, default: false)"
        )
    )]
    pub async fn orgs_add(
        &self,
        org: String,
        ssh: Option<crate::config::OrgSshKeys>,
        workspace_path: Option<String>,
        dry_run: Option<bool>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        let is_dry_run = dry_run.unwrap_or(false);

        stream! {
            // 1. Validate the org name.
            //
            // Reject empty, any path-separator char, any ".." component, any
            // null byte, or any whitespace. The orgs TOML lives on disk at
            // <config_dir>/orgs/<org>.toml — containment in that directory
            // is a safety property.
            let is_invalid = org.is_empty()
                || org.contains('/')
                || org.contains('\\')
                || org.contains('\0')
                || org.contains("..")
                || org.chars().any(|c| c.is_whitespace());

            if is_invalid {
                yield HyperforgeEvent::OrgAddFailed {
                    org: org.clone(),
                    reason: OrgAddFailureReason::InvalidName,
                    message: format!(
                        "Invalid org name {:?}: must be non-empty and must not contain path separators, '..', whitespace, or null bytes.",
                        org,
                    ),
                };
                return;
            }

            // 2. Reject if the org config already exists.
            let config_path = OrgConfig::config_path(&config_dir, &org);
            if config_path.exists() {
                yield HyperforgeEvent::OrgAddFailed {
                    org: org.clone(),
                    reason: OrgAddFailureReason::AlreadyExists,
                    message: format!(
                        "Organization '{}' already exists at {}. Use orgs_update to modify, or orgs_delete then orgs_add to replace.",
                        org,
                        config_path.display(),
                    ),
                };
                return;
            }

            // 3. Build the new config from the typed record.
            let mut new_config = OrgConfig::default();
            if let Some(keys) = ssh {
                new_config.ssh = keys.into_map();
            }
            if let Some(wp) = workspace_path {
                new_config.workspace_path = Some(wp);
            }

            // 4. Dry-run: no write.
            if is_dry_run {
                yield HyperforgeEvent::OrgAdded {
                    org: org.clone(),
                    dry_run: true,
                };
                return;
            }

            // 5. Persist.
            if let Err(e) = new_config.save(&config_dir, &org) {
                yield HyperforgeEvent::OrgAddFailed {
                    org: org.clone(),
                    reason: OrgAddFailureReason::IoError,
                    message: format!("Failed to save org config: {}", e),
                };
                return;
            }

            yield HyperforgeEvent::OrgAdded {
                org: org.clone(),
                dry_run: false,
            };
        }
    }

    /// Update an existing organization — mutate ssh keys and/or workspace_path
    #[plexus_macros::method(
        description = "Update an existing org's ssh map and/or workspace_path. For ssh, the default is per-field merge; pass --replace true for whole-record replacement. For workspace_path: omit to keep current, pass an empty string to clear, or a non-empty value to set.",
        params(
            org = "Organization name (must already exist)",
            ssh = "SSH key paths per forge (optional). One field per known forge — Synapse emits per-forge flags like --ssh.github.",
            workspace_path = "Workspace path — omit to keep current, empty string clears, non-empty sets (optional)",
            replace = "For ssh: replace the whole record instead of per-field merge (optional, default: false)",
            dry_run = "Preview without writing state (optional, default: false)"
        )
    )]
    pub async fn orgs_update(
        &self,
        org: String,
        ssh: Option<crate::config::OrgSshKeys>,
        workspace_path: Option<String>,
        replace: Option<bool>,
        dry_run: Option<bool>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        let is_dry_run = dry_run.unwrap_or(false);
        let do_replace = replace.unwrap_or(false);

        stream! {
            // 1. Require the org to exist. `OrgConfig::load` returns a
            //    default on missing, which would silently create-on-update —
            //    so check the file directly.
            let config_path = OrgConfig::config_path(&config_dir, &org);
            if !config_path.exists() {
                yield HyperforgeEvent::OrgUpdateFailed {
                    org: org.clone(),
                    reason: OrgUpdateFailureReason::NotFound,
                    message: format!(
                        "Organization '{}' not found at {}. Use orgs_add to create it.",
                        org, config_path.display(),
                    ),
                };
                return;
            }

            // 2. Nothing to do? Three-intent semantics for workspace_path
            //    mean "Some(anything)" counts as an update (including
            //    Some("") = clear). A missing ssh record with no
            //    workspace_path is the only no-op case.
            //
            //    Note: --replace true with no ssh field is still
            //    meaningful — it wipes the existing ssh map. But we only
            //    honour --replace when the caller actually provided an
            //    ssh parameter (even an empty one). That way, a caller
            //    who omits ssh entirely cannot accidentally wipe the
            //    stored keys just by setting replace=true.
            if ssh.is_none() && workspace_path.is_none() {
                yield HyperforgeEvent::OrgUpdateFailed {
                    org: org.clone(),
                    reason: OrgUpdateFailureReason::NoFieldsToUpdate,
                    message: format!(
                        "No mutation fields supplied for org '{}'. Provide ssh and/or workspace_path.",
                        org,
                    ),
                };
                return;
            }

            // 3. Load existing config, compute new state, record operations.
            let mut current = OrgConfig::load(&config_dir, &org);
            let mut operations: Vec<OrgUpdateOp> = Vec::new();

            if let Some(keys) = ssh {
                if do_replace {
                    current.ssh = keys.into_map();
                    operations.push(OrgUpdateOp::SshReplaced);
                } else {
                    for (forge, path) in keys.iter() {
                        current.ssh.insert(forge.to_string(), path.to_string());
                    }
                    operations.push(OrgUpdateOp::SshMerged);
                }
            }

            if let Some(wp) = workspace_path {
                if wp.is_empty() {
                    current.workspace_path = None;
                    operations.push(OrgUpdateOp::WorkspacePathCleared);
                } else {
                    current.workspace_path = Some(wp);
                    operations.push(OrgUpdateOp::WorkspacePathSet);
                }
            }

            // 4. Dry-run: report what would happen, do not write.
            if is_dry_run {
                yield HyperforgeEvent::OrgUpdated {
                    org: org.clone(),
                    dry_run: true,
                    operations,
                };
                return;
            }

            // 5. Persist.
            if let Err(e) = current.save(&config_dir, &org) {
                yield HyperforgeEvent::OrgUpdateFailed {
                    org: org.clone(),
                    reason: OrgUpdateFailureReason::IoError,
                    message: format!("Failed to save org config: {}", e),
                };
                return;
            }

            yield HyperforgeEvent::OrgUpdated {
                org: org.clone(),
                dry_run: false,
                operations,
            };
        }
    }

    /// Delete an organization — removes config, keys, repos.yaml, and optionally workspace dir
    #[plexus_macros::method(
        description = "Delete an organization — removes org config, SSH keys, LocalForge data, and optionally the workspace directory. Dry-run by default; pass --confirm true to actually delete.",
        params(
            org = "Organization name to delete",
            remove_workspace = "Also remove the workspace directory (optional, default: false)",
            confirm = "Actually delete (optional, default: false — dry-run unless confirmed)"
        )
    )]
    pub async fn orgs_delete(
        &self,
        org: String,
        remove_workspace: Option<bool>,
        confirm: Option<bool>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        let state = self.state.clone();
        let is_dry_run = !confirm.unwrap_or(false);
        let do_remove_workspace = remove_workspace.unwrap_or(false);

        stream! {
            let dry_prefix = if is_dry_run { "[dry-run] " } else { "" };
            let org_config = OrgConfig::load(&config_dir, &org);

            // Check the org actually exists
            let config_path = OrgConfig::config_path(&config_dir, &org);
            let org_data_dir = config_dir.join("orgs").join(&org);
            if !config_path.exists() && !org_data_dir.exists() {
                yield HyperforgeEvent::Error {
                    message: format!("Organization '{org}' not found."),
                };
                return;
            }

            yield HyperforgeEvent::Info {
                message: format!("{dry_prefix}Deleting organization '{org}'..."),
            };

            // 1. Remove org config TOML
            if config_path.exists() {
                yield HyperforgeEvent::Info {
                    message: format!("  {}Remove config: {}", dry_prefix, config_path.display()),
                };
                if !is_dry_run {
                    if let Err(e) = std::fs::remove_file(&config_path) {
                        yield HyperforgeEvent::Error {
                            message: format!("  Failed to remove config: {e}"),
                        };
                    }
                }
            }

            // 2. Remove org data dir (keys/ + repos.yaml)
            if org_data_dir.exists() {
                yield HyperforgeEvent::Info {
                    message: format!("  {}Remove data dir: {}", dry_prefix, org_data_dir.display()),
                };
                if !is_dry_run {
                    if let Err(e) = std::fs::remove_dir_all(&org_data_dir) {
                        yield HyperforgeEvent::Error {
                            message: format!("  Failed to remove data dir: {e}"),
                        };
                    }
                }
            }

            // 3. Remove workspace dir if requested
            if do_remove_workspace {
                if let Some(ref wp) = org_config.workspace_path {
                    let wp_path = std::path::PathBuf::from(wp);
                    if wp_path.exists() {
                        yield HyperforgeEvent::Info {
                            message: format!("  {dry_prefix}Remove workspace: {wp}"),
                        };
                        if !is_dry_run {
                            if let Err(e) = std::fs::remove_dir_all(&wp_path) {
                                yield HyperforgeEvent::Error {
                                    message: format!("  Failed to remove workspace dir: {e}"),
                                };
                            }
                        }
                    }
                } else {
                    yield HyperforgeEvent::Info {
                        message: "  No workspace path configured — nothing to remove.".to_string(),
                    };
                }
            }

            // 4. Evict from in-memory cache
            if !is_dry_run {
                state.evict_org(&org).await;
            }

            yield HyperforgeEvent::Info {
                message: format!("{dry_prefix}Organization '{org}' deleted."),
            };
        }
    }

    /// Derive needed credentials from workspace dist configs and check which are present
    #[plexus_macros::method(
        description = "Derive needed credentials from workspace dist configs and check which are present",
        params(
            path = "Workspace path (required)",
            include = "Repo name filter (optional, repeatable)",
            exclude = "Repo name filter (optional, repeatable)"
        )
    )]
    pub async fn auth_requirements(
        &self,
        path: String,
        include: Option<Vec<String>>,
        exclude: Option<Vec<String>>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        stream! {
            let workspace_path = std::path::PathBuf::from(&path);
            let ctx = match discover_or_bail(&workspace_path) {
                Ok(ctx) => ctx,
                Err(event) => {
                    yield event;
                    return;
                }
            };

            yield HyperforgeEvent::Info {
                message: format!("Scanning workspace at {}...", ctx.root.display()),
            };

            let filter = RepoFilter::new(include, exclude);

            // Collect credentials needed across all matching repos
            // Key: key_path -> (ResolvedCredential, Vec<repo_names_needing_it>)
            let mut cred_map: std::collections::HashMap<String, (crate::auth::credentials::ResolvedCredential, Vec<String>)> =
                std::collections::HashMap::new();

            let mut matched_repos = 0usize;
            let mut orgs_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            let mut forges_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            let mut channels_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

            for repo in &ctx.repos {
                if !filter.matches(&repo.dir_name) {
                    continue;
                }

                let config = match &repo.config {
                    Some(c) => c,
                    None => continue,
                };

                matched_repos += 1;

                let org = match &config.org {
                    Some(o) => o.clone(),
                    None => continue,
                };

                orgs_seen.insert(org.clone());

                // Collect forge credentials
                for forge_str in &config.forges {
                    forges_seen.insert(forge_str.clone());
                    if let Some(forge_enum) = HyperforgeConfig::parse_forge(forge_str) {
                        let forge_creds = credentials_for_forge(&forge_enum, &org);
                        for cred in forge_creds {
                            let entry = cred_map
                                .entry(cred.key_path.clone())
                                .or_insert_with(|| (cred.clone(), Vec::new()));
                            if !entry.1.contains(&repo.dir_name) {
                                entry.1.push(repo.dir_name.clone());
                            }
                        }
                    }
                }

                // Collect dist channel credentials
                if let Some(ref dist) = config.dist {
                    for channel in &dist.channels {
                        channels_seen.insert(channel.to_string());
                    }

                    if !dist.channels.is_empty() {
                        let channel_creds = credentials_for_channels(&dist.channels, &org);
                        for cred in channel_creds {
                            let entry = cred_map
                                .entry(cred.key_path.clone())
                                .or_insert_with(|| (cred.clone(), Vec::new()));
                            if !entry.1.contains(&repo.dir_name) {
                                entry.1.push(repo.dir_name.clone());
                            }
                        }
                    }
                }
            }

            if cred_map.is_empty() {
                yield HyperforgeEvent::Info {
                    message: format!(
                        "No credentials needed for {matched_repos} repo(s). No forges or dist channels configured.",
                    ),
                };
                return;
            }

            yield HyperforgeEvent::Info {
                message: format!(
                    "Credentials needed for {} repos across {} org(s), {} forge(s), {} channel(s):",
                    matched_repos,
                    orgs_seen.len(),
                    forges_seen.len(),
                    channels_seen.len(),
                ),
            };
            yield HyperforgeEvent::Info { message: String::new() };

            // Check each credential against the secrets store
            let auth = match YamlAuthProvider::new() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to create auth provider: {e}"),
                    };
                    return;
                }
            };

            // Sort credentials for deterministic output
            let mut cred_entries: Vec<_> = cred_map.into_iter().collect();
            cred_entries.sort_by(|a, b| a.0.cmp(&b.0));

            let total = cred_entries.len();
            let mut present_count = 0usize;

            for (key_path, (cred, repo_names)) in &cred_entries {
                let exists = match auth.get_secret(key_path).await {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(_) => false,
                };

                if exists {
                    present_count += 1;
                }

                let status_icon = if exists { "\u{2713}" } else { "\u{2717}" };
                let status_label = if exists { "present" } else { "MISSING" };

                // Build the "needed by" description
                let needed_by = if repo_names.len() <= 3 {
                    repo_names.join(", ")
                } else {
                    format!(
                        "{} repos with {}",
                        repo_names.len(),
                        cred.spec.required_by.join("/"),
                    )
                };

                yield HyperforgeEvent::Info {
                    message: format!(
                        "  {status_icon} {key_path} \u{2014} {status_label} (needed by: {needed_by})",
                    ),
                };
            }

            let missing_count = total - present_count;
            yield HyperforgeEvent::Info { message: String::new() };
            yield HyperforgeEvent::Info {
                message: format!(
                    "{} of {} credentials configured.{}",
                    present_count,
                    total,
                    if missing_count > 0 {
                        let org_hint = orgs_seen.iter().next().map_or("ORG", std::string::String::as_str);
                        format!(" Run `auth setup --org {org_hint}` to set up missing ones.")
                    } else {
                        " All credentials present.".to_string()
                    },
                ),
            };
        }
    }

    /// Guided credential setup — shows what tokens are needed, where to create them, and how to store them
    #[plexus_macros::method(
        description = "Guided credential setup — shows what tokens are needed, where to create them, and how to store them",
        params(
            org = "Organization name (required)",
            forge = "Set up credentials for a specific forge: github, codeberg, or gitlab (optional, sets up all configured forges if omitted)",
            channel = "Set up credentials for specific dist channels (optional, repeatable)"
        )
    )]
    pub async fn auth_setup(
        &self,
        org: String,
        forge: Option<Forge>,
        channel: Option<Vec<DistChannel>>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        let state = self.state.clone();

        stream! {
            const SECRETS_PORT: u16 = 44105;

            // 1. Determine which forges to set up
            let target_forges: Vec<Forge> = if let Some(ref f) = forge {
                vec![f.clone()]
            } else {
                // Discover forges from OrgConfig SSH keys
                let org_config = OrgConfig::load(&config_dir, &org);
                let mut forges: Vec<Forge> = org_config
                    .ssh
                    .keys()
                    .filter_map(|k| HyperforgeConfig::parse_forge(k))
                    .collect();

                // If no SSH keys configured, also check LocalForge records for present_on
                if forges.is_empty() {
                    let local = state.get_local_forge(&org).await;
                    if let Ok(records) = local.all_records() {
                        let mut seen = std::collections::HashSet::new();
                        for rec in &records {
                            for f in &rec.present_on {
                                if seen.insert(f.clone()) {
                                    forges.push(f.clone());
                                }
                            }
                        }
                    }
                }

                if forges.is_empty() {
                    yield HyperforgeEvent::Error {
                        message: format!(
                            "No forges configured for org '{org}'. Run `begin` first or specify --forge."
                        ),
                    };
                    return;
                }

                forges
            };

            // 2. Channel parameter is already typed
            let target_channels: Vec<DistChannel> = channel.unwrap_or_default();

            // 3. Gather credentials — forge tokens + channel tokens
            let mut all_creds: Vec<ResolvedCredential> = Vec::new();

            for forge_enum in &target_forges {
                let forge_creds = credentials_for_forge(forge_enum, &org);
                for cred in forge_creds {
                    if !all_creds.iter().any(|c| c.key_path == cred.key_path) {
                        all_creds.push(cred);
                    }
                }
            }

            if !target_channels.is_empty() {
                let channel_creds = credentials_for_channels(&target_channels, &org);
                for cred in channel_creds {
                    if !all_creds.iter().any(|c| c.key_path == cred.key_path) {
                        all_creds.push(cred);
                    }
                }
            }

            if all_creds.is_empty() {
                yield HyperforgeEvent::Info {
                    message: "No credentials needed for the specified configuration.".to_string(),
                };
                return;
            }

            // 4. Load secrets store to check existence
            let storage = match YamlStorage::default_location() {
                Ok(s) => s,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to open secrets store: {e}"),
                    };
                    return;
                }
            };
            if let Err(e) = storage.load().await {
                yield HyperforgeEvent::Error {
                    message: format!("Failed to load secrets store: {e}"),
                };
                return;
            }

            let forge_names: Vec<String> = target_forges
                .iter()
                .map(|f| match f {
                    Forge::GitHub => "github".to_string(),
                    Forge::Codeberg => "codeberg".to_string(),
                    Forge::GitLab => "gitlab".to_string(),
                })
                .collect();

            yield HyperforgeEvent::Info {
                message: format!(
                    "Setting up credentials for {} on {}...",
                    org,
                    forge_names.join(", "),
                ),
            };
            yield HyperforgeEvent::Info { message: String::new() };

            // 5. Check each credential and emit guidance
            let mut configured_count = 0usize;
            let mut missing_count = 0usize;

            for cred in &all_creds {
                let secret_path = SecretPath::new(&cred.key_path);
                let is_present = storage.exists(&secret_path).unwrap_or(false);

                if is_present {
                    configured_count += 1;
                    yield HyperforgeEvent::Info {
                        message: format!(
                            "\u{2713} {} \u{2014} valid",
                            cred.spec.display_name,
                        ),
                    };
                } else {
                    missing_count += 1;

                    yield HyperforgeEvent::Info {
                        message: format!(
                            "\u{2717} {} \u{2014} MISSING",
                            cred.spec.display_name,
                        ),
                    };

                    // What it's needed for
                    yield HyperforgeEvent::Info {
                        message: format!(
                            "  Needed for: {}",
                            cred.spec.required_by.join(", "),
                        ),
                    };

                    // Special notes for credential kind
                    match &cred.spec.kind {
                        CredentialKind::ClassicPat { required_scopes } => {
                            yield HyperforgeEvent::Info {
                                message: "  Type: Classic Personal Access Token (NOT fine-grained)".to_string(),
                            };
                            yield HyperforgeEvent::Info {
                                message: format!(
                                    "  Required scopes: {}",
                                    required_scopes.join(", "),
                                ),
                            };
                        }
                        CredentialKind::BearerToken => {
                            yield HyperforgeEvent::Info {
                                message: format!("  Type: {}", cred.spec.instructions),
                            };
                        }
                        CredentialKind::ApiKey => {
                            yield HyperforgeEvent::Info {
                                message: format!("  Type: {}", cred.spec.instructions),
                            };
                        }
                        CredentialKind::UsernamePassword => {
                            yield HyperforgeEvent::Info {
                                message: format!("  Type: {}", cred.spec.instructions),
                            };
                        }
                    }

                    // Setup URL
                    yield HyperforgeEvent::Info {
                        message: format!("  Create at: {}", cred.spec.setup_url),
                    };

                    // Synapse command to store it
                    yield HyperforgeEvent::Info {
                        message: "  Then run:".to_string(),
                    };
                    yield HyperforgeEvent::Info {
                        message: format!(
                            "    synapse -P {} secrets auth set_secret --secret_key \"{}\" --value \"$(pbpaste)\"",
                            SECRETS_PORT,
                            cred.key_path,
                        ),
                    };
                    yield HyperforgeEvent::Info { message: String::new() };
                }
            }

            // Summary
            yield HyperforgeEvent::Info { message: String::new() };
            yield HyperforgeEvent::Info {
                message: format!(
                    "{configured_count} configured, {missing_count} need setup",
                ),
            };
        }
    }

    /// Validate all configured tokens — check existence, validity, and scopes
    #[plexus_macros::method(
        description = "Validate all configured tokens — check existence, validity, and scopes",
        params(
            org = "Check credentials for a specific org (optional, checks all if omitted)",
            forge = "Check a specific forge only: github, codeberg, or gitlab (optional)",
            channel = "Check credentials for specific dist channels (optional, repeatable)"
        )
    )]
    pub async fn auth_check(
        &self,
        org: Option<String>,
        forge: Option<Forge>,
        channel: Option<Vec<DistChannel>>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        stream! {
            // Initialize storage and load secrets from disk
            let storage = match YamlStorage::default_location() {
                Ok(s) => s,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to initialize secrets storage: {e}"),
                    };
                    return;
                }
            };
            if let Err(e) = storage.load().await {
                yield HyperforgeEvent::Error {
                    message: format!("Failed to load secrets: {e}"),
                };
                return;
            }

            // Enumerate orgs
            let orgs: Vec<String> = if let Some(ref o) = org {
                vec![o.clone()]
            } else {
                let orgs_dir = config_dir.join("orgs");
                let mut found = Vec::new();
                if orgs_dir.exists() {
                    if let Ok(entries) = std::fs::read_dir(&orgs_dir) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.extension().is_some_and(|e| e == "toml") {
                                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                                    found.push(stem.to_string());
                                }
                            }
                        }
                    }
                }
                found.sort();
                found
            };

            if orgs.is_empty() {
                yield HyperforgeEvent::Info {
                    message: "No organizations configured. Run 'begin' first.".to_string(),
                };
                return;
            }

            // Forge and channel are already typed
            let forge_filter: Option<Forge> = forge;
            let channel_filter: Option<Vec<DistChannel>> = channel;

            // Collect all resolved credentials, dedup by key_path
            let mut all_creds: Vec<ResolvedCredential> = Vec::new();
            let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();

            for org_name in &orgs {
                let org_config = OrgConfig::load(&config_dir, org_name);

                // Determine which forges to check for this org
                let forges_to_check: Vec<Forge> = if let Some(ref ff) = forge_filter {
                    vec![ff.clone()]
                } else {
                    org_config
                        .ssh
                        .keys()
                        .filter_map(|k| HyperforgeConfig::parse_forge(k))
                        .collect()
                };

                // Get forge credentials
                for f in &forges_to_check {
                    for cred in credentials_for_forge(f, org_name) {
                        if seen_keys.insert(cred.key_path.clone()) {
                            all_creds.push(cred);
                        }
                    }
                }

                // Get channel credentials
                if let Some(ref channels) = channel_filter {
                    for cred in credentials_for_channels(channels, org_name) {
                        if seen_keys.insert(cred.key_path.clone()) {
                            all_creds.push(cred);
                        }
                    }
                }
            }

            if all_creds.is_empty() {
                yield HyperforgeEvent::Info {
                    message: "No credentials to check. Configure forges with SSH keys or specify channels.".to_string(),
                };
                return;
            }

            yield HyperforgeEvent::Info {
                message: format!("Checking {} credential(s)...", all_creds.len()),
            };

            let mut valid = 0usize;
            let mut missing = 0usize;
            let mut invalid = 0usize;
            let total = all_creds.len();

            for cred in &all_creds {
                let secret_path = SecretPath::new(&cred.key_path);
                let token = match storage.get(&secret_path) {
                    Ok(secret) if !secret.value.is_empty() => secret.value,
                    _ => {
                        missing += 1;
                        yield HyperforgeEvent::AuthCheckResult {
                            credential: cred.spec.display_name.to_string(),
                            key_path: cred.key_path.clone(),
                            status: "missing".to_string(),
                            detail: Some(format!("Set up at: {}", cred.spec.setup_url)),
                        };
                        continue;
                    }
                };

                let (status, detail) = validate_credential(cred.spec, &token).await;

                match status.as_str() {
                    "valid" => valid += 1,
                    _ => invalid += 1,
                }

                yield HyperforgeEvent::AuthCheckResult {
                    credential: cred.spec.display_name.to_string(),
                    key_path: cred.key_path.clone(),
                    status,
                    detail,
                };
            }

            yield HyperforgeEvent::Info {
                message: format!(
                    "{valid} valid, {missing} missing, {invalid} invalid out of {total} credentials checked",
                ),
            };
        }
    }

}

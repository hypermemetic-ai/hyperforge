//! `WorkspacesService` — v5 workspaces namespace.
//!
//! V5CORE-8 shipped the empty static child. V5WS-2 attaches the first
//! real method (`list`); subsequent V5WS tickets layer in the rest of
//! the workspace lifecycle.
//!
//! Writes are atomic per D8; mutating methods default `dry_run: false`
//! per D7; error events follow D9 (`type: "error"` plus `code` and
//! `message`).

use std::path::PathBuf;

use async_stream::stream;
use futures::Stream;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v5::config::{load_workspaces, WorkspaceRepo};

/// Workspaces namespace. Each method reads/writes `workspaces/*.yaml`
/// under the daemon's config directory.
#[derive(Clone)]
pub struct WorkspacesService {
    config_dir: PathBuf,
}

impl WorkspacesService {
    #[must_use]
    pub const fn new(config_dir: PathBuf) -> Self {
        Self { config_dir }
    }
}

/// Events emitted by `WorkspacesService` methods.
///
/// Serialized with `#[serde(tag = "type")]` per D9; every variant maps
/// to a `type` discriminator in `snake_case`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspacesEvent {
    /// One checkout, main or worktree (`orient`).
    OrientCheckout { row: OrientRow },
    /// `orient --table`: aligned text, one line per checkout.
    OrientTable { text: String },
    /// One summary per workspace (V5WS-2 `list`).
    WorkspaceSummary {
        name: String,
        path: String,
        repo_count: u32,
    },
    /// Full workspace detail (V5WS-3 `get`). `repos` preserves each
    /// entry's original on-disk shape (string shorthand or `{ref,dir}`
    /// object) and source order.
    WorkspaceDetail {
        name: String,
        path: String,
        repos: Vec<WorkspaceRepo>,
    },
    /// Delete acknowledgement (V5WS-5 `delete`).
    WorkspaceDeleted { name: String },
    /// Per-member cascade event for `delete_remote: true` flows.
    /// `status` is `forge_deleted` or `forge_delete_failed`; `message`
    /// is a free-text diagnostic on failure.
    ForgeDeleteResult {
        #[serde(rename = "ref")]
        reference: crate::v5::config::RepoRef,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// One reconcile observation (V5WS-8). `kind` is one of
    /// `matched`, `renamed`, `removed`, `new_matched`, `ambiguous`.
    ReconcileEvent {
        kind: String,
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        reference: Option<crate::v5::config::RepoRef>,
        #[serde(skip_serializing_if = "Option::is_none")]
        dir: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Generic error. `code` is a `snake_case` discriminator drawn from
    /// the emitting method's closed error set.
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        message: String,
    },
    /// V5WS-9: per-member sync result. Shape parallels
    /// `RepoEvent::SyncDiff` from V5REPOS-13; one event per workspace
    /// member repo.
    SyncDiff {
        #[serde(rename = "ref")]
        reference: crate::v5::repos::RepoRefWire,
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        status: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        drift: Vec<crate::v5::repos::DriftField>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_class: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        remote: Option<crate::v5::adapters::ForgeMetadata>,
    },
    /// V5WS-9 + V5PROV-8 + V5LIFECYCLE-10: aggregate across members.
    /// `created` counts members that were absent on the forge and
    /// created by this sync call. `skipped` counts members skipped
    /// (e.g. `lifecycle: dismissed` without `include_dismissed`).
    /// Invariant: `total == in_sync + drifted + errored + created + skipped`.
    WorkspaceSyncReport {
        name: String,
        total: u32,
        in_sync: u32,
        drifted: u32,
        errored: u32,
        #[serde(default)]
        created: u32,
        #[serde(default)]
        skipped: u32,
        per_repo: Vec<serde_json::Value>,
    },
    /// V5LIFECYCLE-10: a member was skipped in this sync pass.
    SyncSkipped {
        #[serde(rename = "ref")]
        reference: crate::v5::repos::RepoRefWire,
        reason: String,
    },
    /// V5LIFECYCLE-10: a local dir's `.hyperforge/config.toml`
    /// declares a different identity than the workspace assigns.
    /// Informational only — the org yaml remains authoritative.
    ConfigDrift {
        dir: String,
        declared_org: String,
        declared_repo: String,
        workspace_org: String,
        workspace_repo: String,
    },
    /// V5PARITY-3: per-member result from a workspace-level git op.
    MemberGitResult {
        #[serde(rename = "ref")]
        reference: crate::v5::repos::RepoRefWire,
        op: String, // "clone" | "fetch" | "pull" | "push_refs"
        status: String, // "ok" | "errored"
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// V5PARITY-3: aggregate emitted after a workspace-level git op.
    WorkspaceGitSummary {
        name: String,
        op: String,
        total: u32,
        ok: u32,
        errored: u32,
        /// HYPERFORGE-64 (3): git clones directly under the workspace path
        /// that are not members (a `pull` names them; nothing is done to
        /// them). Linked worktrees of members are not counted.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        not_members: Vec<String>,
        /// A sentence for a reader: what `not_members` means.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// V5PARITY-2: per-dir result during `workspaces.discover`.
    /// `status` is `matched` (dir's origin resolves to a known repo),
    /// `orphan` (unknown origin), or `already_member` (a workspace
    /// already registered this ref).
    DiscoverMatch {
        dir: String,
        status: String,
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        reference: Option<crate::v5::repos::RepoRefWire>,
        #[serde(skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
    },
    /// V5PARITY-2: emitted once after a discover pass creates or
    /// updates a workspace yaml.
    WorkspaceDiscovered {
        name: String,
        path: String,
        repo_count: u32,
    },
    /// V5PARITY-22: workspace created via `from_org`.
    WorkspaceCreated {
        name: String,
        path: String,
        org: String,
    },
    /// V5PARITY-22: per-member add result during `from_org`.
    MemberAdded {
        #[serde(rename = "ref")]
        reference: crate::v5::repos::RepoRefWire,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        already_present: bool,
    },
    /// V5PARITY-35: per-member result of `workspaces.set_forges`.
    ForgesSet {
        #[serde(rename = "ref")]
        reference: crate::v5::repos::RepoRefWire,
        forges: Option<Vec<String>>,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        changed: bool,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        local_path: Option<String>,
    },
    /// V5PARITY-35: aggregate of `workspaces.set_forges`.
    WorkspaceSetForgesSummary {
        name: String,
        total: u32,
        ok: u32,
        errored: u32,
        unchanged: u32,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        dry_run: bool,
    },
    /// V5PARITY-4: per-member analytics event (size/loc/large/dirty).
    /// `metric` is `size`, `loc`, `large_files`, or `dirty`; fields
    /// populated depend on the metric.
    MemberAnalytics {
        #[serde(rename = "ref")]
        reference: crate::v5::repos::RepoRefWire,
        metric: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        bytes: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_count: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        loc_total: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        large_count: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        dirty: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// V5PARITY-14: per-member git status snapshot. Fields mirror
    /// V5PARITY-3's `RepoEvent::RepoStatus`; no central bookkeeping
    /// shape because workspace status is fundamentally read-only and
    /// the per-member detail is the user-facing payload.
    StatusSnapshot {
        #[serde(rename = "ref")]
        reference: crate::v5::repos::RepoRefWire,
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
    /// V5PARITY-14: workspace-wide status aggregate.
    WorkspaceStatusSummary {
        name: String,
        total: u32,
        clean: u32,
        dirty: u32,
        ahead: u32,
        behind: u32,
        errored: u32,
    },
    /// Per-member result of `workspaces.migrate_org`: this member's
    /// repo was moved to `new_org` (membership + config.toml + remotes).
    MemberMigrated {
        #[serde(rename = "ref")]
        reference: crate::v5::repos::RepoRefWire,
        old_org: String,
        new_org: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        local_path: Option<String>,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        dry_run: bool,
    },
    /// Aggregate emitted after `workspaces.migrate_org`. Invariant:
    /// `total == succeeded + errored`.
    WorkspaceMigrateOrgSummary {
        name: String,
        total: u32,
        succeeded: u32,
        errored: u32,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        dry_run: bool,
    },
    /// Per-repo result of `workspaces.org_diff`: where this member's repo
    /// actually lives on the forge right now. `declared_org` is what the
    /// workspace/org yaml claims; `canonical_org` is the forge truth
    /// (rename/transfer-following). `verdict` is one of `moved`, `stayed`,
    /// `elsewhere`, `missing`.
    OrgDiffEntry {
        name: String,
        declared_org: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        canonical_org: Option<String>,
        verdict: String,
    },
    /// Aggregate of `workspaces.org_diff`. The `moved` list is the EXACT
    /// `members` argument to pass to `workspaces.migrate_org` to migrate
    /// only the repos that actually moved on the forge — composing the two
    /// makes a partial org migration safe (no over-migrating the repos
    /// that stayed). Invariant:
    /// `total == moved.len() + stayed.len() + elsewhere.len() + missing.len()`.
    OrgDiffSummary {
        from_org: String,
        to_org: String,
        total: u32,
        moved: Vec<String>,
        stayed: Vec<String>,
        elsewhere: Vec<String>,
        missing: Vec<String>,
    },
    /// V5PARITY-4: workspace-wide aggregate.
    WorkspaceAnalyticsSummary {
        name: String,
        metric: String,
        total: u32,
        ok: u32,
        errored: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        total_bytes: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        total_file_count: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        total_loc: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        total_large_files: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        dirty_count: Option<u32>,
    },
}

/// Validate a `WorkspaceName`: ≤64 chars, ASCII, no `/`, no leading `.`.
fn is_valid_name(s: &str) -> bool {
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    if s.starts_with('.') {
        return false;
    }
    if !s.is_ascii() {
        return false;
    }
    !s.contains('/')
}

/// Validate an `FsPath`: absolute, no `..`, no trailing `/`.
fn is_valid_fspath(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if !s.starts_with('/') {
        return false;
    }
    if s.len() > 1 && s.ends_with('/') {
        return false;
    }
    !s.split('/').any(|seg| seg == "..")
}

/// Parse `<org>/<name>` string form. Returns `None` if the input does
/// not look like exactly one forward slash separating two non-empty
/// tokens that individually pass name validation.
fn parse_repo_ref_string(
    s: &str,
) -> Option<(crate::v5::config::OrgName, crate::v5::config::RepoName)> {
    let (org, name) = s.split_once('/')?;
    if !is_valid_name(org) || !is_valid_name(name) {
        return None;
    }
    Some((org.into(), name.into()))
}

/// Parse a `RepoRef` argument: accepts either `<org>/<name>` string
/// form or a JSON object `{"org":"<o>","name":"<n>"}`.
fn parse_repo_ref_arg(raw: &str) -> Option<crate::v5::config::RepoRef> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        let parsed: serde_json::Value = serde_json::from_str(trimmed).ok()?;
        let org = parsed.get("org")?.as_str()?;
        let name = parsed.get("name")?.as_str()?;
        if is_valid_name(org) && is_valid_name(name) {
            return Some(crate::v5::config::RepoRef {
                org: org.into(),
                name: name.into(),
            });
        }
        return None;
    }
    let (org, name) = parse_repo_ref_string(trimmed)?;
    Some(crate::v5::config::RepoRef { org, name })
}

/// Ref key for comparing `WorkspaceRepo` entries regardless of shape.
fn ref_key(entry: &WorkspaceRepo) -> Option<(String, String)> {
    match entry {
        WorkspaceRepo::Shorthand(s) => {
            let (o, n) = s.split_once('/')?;
            Some((o.to_string(), n.to_string()))
        }
        WorkspaceRepo::Object { reference, .. } => Some((
            reference.org.as_str().to_string(),
            reference.name.as_str().to_string(),
        )),
    }
}

/// Parse the `repos` parameter as a JSON array string. Missing or
/// empty → `Ok(vec![])`. Entries may be bare strings (shorthand
/// `<org>/<name>`) or `{ref, dir}` objects.
fn parse_repos_arg(raw: &Option<String>) -> Result<Vec<WorkspaceRepo>, String> {
    let Some(raw) = raw.as_ref() else {
        return Ok(Vec::new());
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let items: Vec<serde_json::Value> = serde_json::from_str(trimmed)
        .map_err(|e| format!("repos must be a JSON array: {e}"))?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            serde_json::Value::String(s) => out.push(WorkspaceRepo::Shorthand(s)),
            serde_json::Value::Object(_) => {
                let entry: WorkspaceRepo = serde_json::from_value(item)
                    .map_err(|e| format!("malformed object entry: {e}"))?;
                out.push(entry);
            }
            other => return Err(format!("unsupported repos entry: {other}")),
        }
    }
    Ok(out)
}

/// Workspaces CRUD + reconcile + sync.
#[plexus_macros::activation(
    trait = "WorkspacesApi",
    methods = marked,
    namespace = "workspaces",
    description = "Workspaces CRUD",
    crate_path = "plexus_core"
)]
impl WorkspacesService {
    /// List every workspace declared under `$HF_CONFIG/workspaces/`.
    ///
    /// Emits one `workspace_summary` event per parsed yaml file in
    /// ascending-name order. If `workspaces/` is absent the stream is
    /// empty (not an error). Parse failures surface as typed error
    /// events; other workspaces still stream.
    #[plexus_macros::method]
    pub async fn list(&self) -> impl Stream<Item = WorkspacesEvent> + Send + 'static {
        let ws_dir = self.config_dir.join("workspaces");
        stream! {
            if !ws_dir.is_dir() {
                return;
            }
            match load_workspaces(&ws_dir) {
                Ok(map) => {
                    for (_name, cfg) in map {
                        let count = u32::try_from(cfg.repos.len()).unwrap_or(u32::MAX);
                        yield WorkspacesEvent::WorkspaceSummary {
                            name: cfg.name.as_str().to_string(),
                            path: cfg.path.as_str().to_string(),
                            repo_count: count,
                        };
                    }
                }
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("invalid_config".into()),
                        message: e.to_string(),
                    };
                }
            }
        }
    }

    /// Return one workspace's full detail.
    ///
    /// Emits a single `workspace_detail` event whose `repos` preserves
    /// the on-disk mix of string-form and object-form entries in
    /// source order. Missing workspace → typed not-found.
    #[plexus_macros::method(params(name = "Workspace name to fetch"))]
    pub async fn get(
        &self,
        name: String,
    ) -> impl Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        let ws_dir = config_dir.join("workspaces");
        stream! {
            if name.is_empty() {
                yield WorkspacesEvent::Error {
                    code: Some("missing_param".into()),
                    message: "required parameter 'name' is missing".into(),
                };
                return;
            }
            if !is_valid_name(&name) {
                yield WorkspacesEvent::Error {
                    code: Some("invalid_name".into()),
                    message: format!("invalid workspace name: {name}"),
                };
                return;
            }
            let cfg = match crate::v5::ops::state::load_workspace(&config_dir, &name) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    yield WorkspacesEvent::Error {
                        code: Some("not_found".into()),
                        message: format!("workspace not found: {name}"),
                    };
                    return;
                }
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let _ = ws_dir; // not needed after ops::state migration
            yield WorkspacesEvent::WorkspaceDetail {
                name: cfg.name.as_str().to_string(),
                path: cfg.path.as_str().to_string(),
                repos: cfg.repos,
            };
        }
    }

    /// Create a new workspace yaml.
    ///
    /// Every repo ref in `repos` must resolve against its
    /// `orgs/<org>.yaml` — unresolved entries abort the write and
    /// surface as a typed error naming every offender. `ws_path` is
    /// named to avoid synapse's path-expansion of a parameter named
    /// `path`. D7: `dry_run` defaults false; on dry runs no file is
    /// written but the same event is emitted. D8: writes are atomic.
    #[plexus_macros::method(params(
        name = "New workspace name (filename-safe)",
        ws_path = "Absolute path where clones will live (named ws_path to avoid synapse path-expansion)",
        repos = "JSON array of repo refs (strings or {ref,dir} objects); default []",
        dry_run = "Preview without writing; default false",
    ))]
    pub async fn create(
        &self,
        name: String,
        ws_path: String,
        repos: Option<String>,
        dry_run: Option<bool>,
    ) -> impl Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        let dry = dry_run.unwrap_or(false);
        stream! {
            if !is_valid_name(&name) {
                yield WorkspacesEvent::Error {
                    code: Some("invalid_name".into()),
                    message: format!("invalid workspace name: {name}"),
                };
                return;
            }
            if !is_valid_fspath(&ws_path) {
                yield WorkspacesEvent::Error {
                    code: Some("invalid_path".into()),
                    message: format!("invalid ws_path: {ws_path}"),
                };
                return;
            }
            let parsed_repos = match parse_repos_arg(&repos) {
                Ok(r) => r,
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("invalid_repos".into()),
                        message: e,
                    };
                    return;
                }
            };
            let ws_dir = config_dir.join("workspaces");
            let target = ws_dir.join(format!("{name}.yaml"));
            if target.exists() {
                yield WorkspacesEvent::Error {
                    code: Some("already_exists".into()),
                    message: format!("workspace already exists: {name}"),
                };
                return;
            }
            // Validate every ref against its org yaml.
            let orgs_dir = config_dir.join("orgs");
            let orgs = match crate::v5::config::load_orgs(&orgs_dir) {
                Ok(m) => m,
                Err(_) if !orgs_dir.exists() => std::collections::BTreeMap::new(),
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("invalid_config".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let mut bad: Vec<String> = Vec::new();
            for entry in &parsed_repos {
                let (org_s, name_s) = match entry {
                    WorkspaceRepo::Shorthand(s) => {
                        let Some((o, n)) = s.split_once('/') else {
                            bad.push(s.clone());
                            continue;
                        };
                        (o.to_string(), n.to_string())
                    }
                    WorkspaceRepo::Object { reference, .. } => (
                        reference.org.as_str().to_string(),
                        reference.name.as_str().to_string(),
                    ),
                };
                let Some(org_cfg) = orgs.get(&org_s.clone().into()) else {
                    bad.push(format!("{org_s}/{name_s}"));
                    continue;
                };
                if !org_cfg.repos.iter().any(|r| r.name.as_str() == name_s) {
                    bad.push(format!("{org_s}/{name_s}"));
                }
            }
            if !bad.is_empty() {
                yield WorkspacesEvent::Error {
                    code: Some("unresolved_ref".into()),
                    message: format!("unresolved repo refs: {}", bad.join(", ")),
                };
                return;
            }
            let count = u32::try_from(parsed_repos.len()).unwrap_or(u32::MAX);
            let cfg = crate::v5::config::WorkspaceConfig {
                name: name.clone().into(),
                path: ws_path.clone().into(),
                repos: parsed_repos,
            };
            if !dry {
                if let Err(e) = std::fs::create_dir_all(&ws_dir) {
                    yield WorkspacesEvent::Error {
                        code: Some("io_error".into()),
                        message: format!("create {}: {e}", ws_dir.display()),
                    };
                    return;
                }
                if let Err(e) = crate::v5::config::save_workspace(&ws_dir, &cfg) {
                    yield WorkspacesEvent::Error {
                        code: Some("io_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            }
            yield WorkspacesEvent::WorkspaceSummary {
                name,
                path: ws_path,
                repo_count: count,
            };
        }
    }

    /// Delete a workspace yaml, optionally cascading to forge-side
    /// repo deletion for each member.
    ///
    /// Cascade mode (`delete_remote: true`) emits one
    /// `forge_delete_result` event per member before the yaml is
    /// removed; the batch continues past per-member failures. v1 has
    /// no adapter path — cascade events report
    /// `forge_delete_failed` on real runs (or `forge_deleted` on
    /// `dry_run: true` so the preview shape is stable). V5REPOS-13
    /// wires the real adapter path; post-V5REPOS this method picks it
    /// up without signature changes.
    #[plexus_macros::method(params(
        name = "Workspace to delete",
        delete_remote = "Cascade forge-side deletion for each member; default false",
        dry_run = "Preview without writing; default false",
    ))]
    pub async fn delete(
        &self,
        name: String,
        delete_remote: Option<bool>,
        dry_run: Option<bool>,
    ) -> impl Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        let dry = dry_run.unwrap_or(false);
        let cascade = delete_remote.unwrap_or(false);
        stream! {
            if !is_valid_name(&name) {
                yield WorkspacesEvent::Error {
                    code: Some("invalid_name".into()),
                    message: format!("invalid workspace name: {name}"),
                };
                return;
            }
            let ws_dir = config_dir.join("workspaces");
            let target = ws_dir.join(format!("{name}.yaml"));
            let cfg = match crate::v5::ops::state::load_workspace(&config_dir, &name) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    yield WorkspacesEvent::Error {
                        code: Some("not_found".into()),
                        message: format!("workspace not found: {name}"),
                    };
                    return;
                }
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let _ = &target; // target path retained for downstream error messages if needed
            if cascade {
                for entry in &cfg.repos {
                    let Some((org, rname)) = ref_key(entry) else { continue };
                    let rref = crate::v5::config::RepoRef {
                        org: org.into(),
                        name: rname.into(),
                    };
                    let (status, msg) = if dry {
                        ("forge_deleted".to_string(), None)
                    } else {
                        (
                            "forge_delete_failed".to_string(),
                            Some("no forge adapter registered (V5REPOS-13 pending)".into()),
                        )
                    };
                    yield WorkspacesEvent::ForgeDeleteResult {
                        reference: rref,
                        status,
                        message: msg,
                    };
                }
            }
            if !dry {
                if let Err(e) = std::fs::remove_file(&target) {
                    yield WorkspacesEvent::Error {
                        code: Some("io_error".into()),
                        message: format!("remove {}: {e}", target.display()),
                    };
                    return;
                }
            }
            yield WorkspacesEvent::WorkspaceDeleted { name };
        }
    }

    /// Append one `<org>/<name>` repo ref to a workspace. The ref must
    /// resolve against `orgs/<org>.yaml`; duplicates (regardless of
    /// the existing entry's shape) fail without a write. Pinned here:
    /// the canonical string form `<org>/<name>` — one forward slash
    /// separating two tokens, each rejecting `/`.
    #[plexus_macros::method(params(
        name = "Workspace to extend",
        repo_ref = "Repo ref in '<org>/<name>' form (or {org,name} object)",
        dry_run = "Preview without writing; default false",
    ))]
    pub async fn add_repo(
        &self,
        name: String,
        repo_ref: String,
        dry_run: Option<bool>,
    ) -> impl Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        let dry = dry_run.unwrap_or(false);
        stream! {
            if !is_valid_name(&name) {
                yield WorkspacesEvent::Error {
                    code: Some("invalid_name".into()),
                    message: format!("invalid workspace name: {name}"),
                };
                return;
            }
            let Some(rref) = parse_repo_ref_arg(&repo_ref) else {
                yield WorkspacesEvent::Error {
                    code: Some("invalid_ref".into()),
                    message: format!("invalid repo_ref: {repo_ref}"),
                };
                return;
            };
            let ws_dir = config_dir.join("workspaces");
            let mut cfg = match crate::v5::ops::state::load_workspace(&config_dir, &name) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    yield WorkspacesEvent::Error {
                        code: Some("not_found".into()),
                        message: format!("workspace not found: {name}"),
                    };
                    return;
                }
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            // Validate against its org yaml via ops::state.
            let orgs = match crate::v5::ops::state::load_orgs(&config_dir.join("orgs")) {
                Ok(o) => o,
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let org_cfg = match orgs.get(&rref.org) {
                Some(o) => o.clone(),
                None => {
                    yield WorkspacesEvent::Error {
                        code: Some("org_not_found".into()),
                        message: format!(
                            "org not found: {} (ref {}/{})",
                            rref.org, rref.org, rref.name
                        ),
                    };
                    return;
                }
            };
            if !org_cfg
                .repos
                .iter()
                .any(|r| r.name.as_str() == rref.name.as_str())
            {
                yield WorkspacesEvent::Error {
                    code: Some("repo_not_found".into()),
                    message: format!("repo not found in org: {}/{}", rref.org, rref.name),
                };
                return;
            }
            let key = (rref.org.as_str().to_string(), rref.name.as_str().to_string());
            let already = cfg
                .repos
                .iter()
                .any(|e| ref_key(e).as_ref() == Some(&key));
            if already {
                yield WorkspacesEvent::Error {
                    code: Some("already_member".into()),
                    message: format!("already a member: {}/{}", rref.org, rref.name),
                };
                return;
            }
            cfg.repos
                .push(WorkspaceRepo::Shorthand(format!("{}/{}", rref.org, rref.name)));
            let count = u32::try_from(cfg.repos.len()).unwrap_or(u32::MAX);
            let path_str = cfg.path.as_str().to_string();
            if !dry {
                if let Err(e) = crate::v5::config::save_workspace(&ws_dir, &cfg) {
                    yield WorkspacesEvent::Error {
                        code: Some("io_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            }
            yield WorkspacesEvent::WorkspaceSummary {
                name,
                path: path_str,
                repo_count: count,
            };
        }
    }

    /// Drop a member ref from a workspace, optionally cascading to
    /// forge-side deletion. An object-form entry `{ref, dir}` is
    /// matched by ref (the `dir` is not material); duplicate entries
    /// drop the first match. Cascade events follow the same v1
    /// skeletal shape as `delete` — real adapter path lands in
    /// V5REPOS-13.
    #[plexus_macros::method(params(
        name = "Workspace to modify",
        repo_ref = "Repo ref in '<org>/<name>' form (or {org,name} object)",
        delete_remote = "Cascade forge-side deletion of the ref; default false",
        dry_run = "Preview without writing; default false",
    ))]
    pub async fn remove_repo(
        &self,
        name: String,
        repo_ref: String,
        delete_remote: Option<bool>,
        dry_run: Option<bool>,
    ) -> impl Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        let dry = dry_run.unwrap_or(false);
        let cascade = delete_remote.unwrap_or(false);
        stream! {
            if !is_valid_name(&name) {
                yield WorkspacesEvent::Error {
                    code: Some("invalid_name".into()),
                    message: format!("invalid workspace name: {name}"),
                };
                return;
            }
            let Some(rref) = parse_repo_ref_arg(&repo_ref) else {
                yield WorkspacesEvent::Error {
                    code: Some("invalid_ref".into()),
                    message: format!("invalid repo_ref: {repo_ref}"),
                };
                return;
            };
            let ws_dir = config_dir.join("workspaces");
            let target = ws_dir.join(format!("{name}.yaml"));
            let mut cfg = match crate::v5::ops::state::load_workspace(&config_dir, &name) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    yield WorkspacesEvent::Error {
                        code: Some("not_found".into()),
                        message: format!("workspace not found: {name}"),
                    };
                    return;
                }
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let _ = &target;
            let key = (rref.org.as_str().to_string(), rref.name.as_str().to_string());
            let idx = cfg
                .repos
                .iter()
                .position(|e| ref_key(e).as_ref() == Some(&key));
            let Some(idx) = idx else {
                yield WorkspacesEvent::Error {
                    code: Some("not_a_member".into()),
                    message: format!("not a member: {}/{}", rref.org, rref.name),
                };
                return;
            };
            if cascade {
                let (status, msg) = if dry {
                    ("forge_deleted".to_string(), None)
                } else {
                    (
                        "forge_delete_failed".to_string(),
                        Some("no forge adapter registered (V5REPOS-13 pending)".into()),
                    )
                };
                yield WorkspacesEvent::ForgeDeleteResult {
                    reference: rref.clone(),
                    status,
                    message: msg,
                };
            }
            cfg.repos.remove(idx);
            let count = u32::try_from(cfg.repos.len()).unwrap_or(u32::MAX);
            let path_str = cfg.path.as_str().to_string();
            if !dry {
                if let Err(e) = crate::v5::config::save_workspace(&ws_dir, &cfg) {
                    yield WorkspacesEvent::Error {
                        code: Some("io_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            }
            yield WorkspacesEvent::WorkspaceSummary {
                name,
                path: path_str,
                repo_count: count,
            };
        }
    }

    /// Reconcile the workspace yaml with disk.
    ///
    /// Scans `path` in ASCII-ascending dir order, reads each git
    /// clone's `origin` URL out of `.git/config`, and matches against
    /// org-yaml remotes. Emits one `reconcile_event` per observation.
    /// Kinds: `matched`, `renamed`, `removed`, `new_matched`,
    /// `ambiguous` (CONTRACTS §types). Under D5, when multiple dirs
    /// share a URL the alphabetically-first wins; other candidates
    /// emit `ambiguous`. Non-dry runs rewrite the workspace yaml
    /// atomically (D8) to reflect `renamed` + `removed`; the scan is
    /// strictly read-only — no filesystem mutation under the
    /// workspace path and no forge contact.
    #[plexus_macros::method(params(
        name = "Workspace to reconcile",
        dry_run = "Preview without writing; default false",
    ))]
    pub async fn reconcile(
        &self,
        name: String,
        dry_run: Option<bool>,
    ) -> impl Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        let dry = dry_run.unwrap_or(false);
        stream! {
            if !is_valid_name(&name) {
                yield WorkspacesEvent::Error {
                    code: Some("invalid_name".into()),
                    message: format!("invalid workspace name: {name}"),
                };
                return;
            }
            let ws_dir = config_dir.join("workspaces");
            let target = ws_dir.join(format!("{name}.yaml"));
            let mut cfg = match crate::v5::ops::state::load_workspace(&config_dir, &name) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    yield WorkspacesEvent::Error {
                        code: Some("not_found".into()),
                        message: format!("workspace not found: {name}"),
                    };
                    return;
                }
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let _ = &target;
            // HYPE-5: global config for owner-alias resolution in the
            // config-drift scan below. Best-effort; missing/invalid → no
            // aliases (identity comparison), preserving prior behavior.
            let global = crate::v5::config::load_global(&config_dir.join("config.yaml"))
                .unwrap_or_default();
            // Load orgs for URL → ref lookup.
            let orgs_dir = config_dir.join("orgs");
            let orgs = match crate::v5::config::load_orgs(&orgs_dir) {
                Ok(m) => m,
                Err(_) if !orgs_dir.exists() => std::collections::BTreeMap::new(),
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("invalid_config".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            // URL → sorted (org, name) candidates. Multiple refs may
            // share a URL (mirror repos); tiebreaker is alphabetical
            // `<org>/<name>`.
            let mut url_to_refs: std::collections::BTreeMap<
                String,
                Vec<(String, String)>,
            > = std::collections::BTreeMap::new();
            for (org_name, org_cfg) in &orgs {
                for repo in &org_cfg.repos {
                    for remote in &repo.remotes {
                        url_to_refs
                            .entry(remote.url.as_str().to_string())
                            .or_default()
                            .push((
                                org_name.as_str().to_string(),
                                repo.name.as_str().to_string(),
                            ));
                    }
                }
            }
            for v in url_to_refs.values_mut() {
                v.sort();
                v.dedup();
            }
            // Per-member info: reference, declared dir, known remotes.
            struct Member {
                reference: crate::v5::config::RepoRef,
                declared_dir: String,
                remote_urls: Vec<String>,
            }
            let mut members: Vec<Member> = Vec::new();
            for entry in &cfg.repos {
                let Some((org_s, name_s)) = ref_key(entry) else { continue };
                let reference = crate::v5::config::RepoRef {
                    org: org_s.clone().into(),
                    name: name_s.clone().into(),
                };
                let declared_dir = match entry {
                    WorkspaceRepo::Shorthand(_) => name_s.clone(),
                    WorkspaceRepo::Object { dir, .. } => dir.clone(),
                };
                let remote_urls = orgs
                    .get(&org_s.clone().into())
                    .and_then(|o| o.repos.iter().find(|r| r.name.as_str() == name_s))
                    .map(|r| {
                        r.remotes
                            .iter()
                            .map(|rem| rem.url.as_str().to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                members.push(Member {
                    reference,
                    declared_dir,
                    remote_urls,
                });
            }
            // Scan the workspace path in ASCII-ascending dir order.
            let scan_root = std::path::PathBuf::from(cfg.path.as_str());
            let mut entries: Vec<(String, std::path::PathBuf)> = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&scan_root) {
                for ent in rd.flatten() {
                    let p = ent.path();
                    if !p.is_dir() {
                        continue;
                    }
                    let Some(nm) = p.file_name().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    entries.push((nm.to_string(), p));
                }
            }
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            // For each dir, resolve its origin URL.
            let mut dir_origin: Vec<(String, Option<String>)> = Vec::new();
            for (dname, dpath) in &entries {
                let origin = read_git_origin(dpath);
                dir_origin.push((dname.clone(), origin));
            }
            // Decisions in alphabetical-by-ref order satisfy D5's
            // "first scanned wins" when multiple dirs match one ref.
            #[derive(Clone)]
            struct MemberDecision {
                idx: usize,
                kind: String,
                dir: Option<String>,
            }
            let mut decisions: Vec<MemberDecision> = Vec::new();
            let mut consumed_dirs: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            let mut order: Vec<usize> = (0..members.len()).collect();
            order.sort_by(|&a, &b| {
                let ka = (
                    members[a].reference.org.as_str(),
                    members[a].reference.name.as_str(),
                );
                let kb = (
                    members[b].reference.org.as_str(),
                    members[b].reference.name.as_str(),
                );
                ka.cmp(&kb)
            });
            for &mi in &order {
                let m = &members[mi];
                let mut candidates: Vec<String> = Vec::new();
                for (dname, origin) in &dir_origin {
                    if consumed_dirs.contains(dname) {
                        continue;
                    }
                    let Some(url) = origin else { continue };
                    if m.remote_urls.iter().any(|u| u == url) {
                        candidates.push(dname.clone());
                    }
                }
                if candidates.is_empty() {
                    decisions.push(MemberDecision {
                        idx: mi,
                        kind: "removed".into(),
                        dir: None,
                    });
                    continue;
                }
                // Prefer declared dir if it's among candidates.
                let (winner, rest): (String, Vec<String>) =
                    if let Some(pos) =
                        candidates.iter().position(|d| *d == m.declared_dir)
                    {
                        let w = candidates.remove(pos);
                        (w, candidates)
                    } else {
                        let mut it = candidates.into_iter();
                        (it.next().unwrap(), it.collect())
                    };
                consumed_dirs.insert(winner.clone());
                let kind = if winner == m.declared_dir {
                    "matched"
                } else {
                    "renamed"
                };
                decisions.push(MemberDecision {
                    idx: mi,
                    kind: kind.into(),
                    dir: Some(winner),
                });
                for other in rest {
                    decisions.push(MemberDecision {
                        idx: mi,
                        kind: "ambiguous".into(),
                        dir: Some(other),
                    });
                }
            }
            // `new_matched`: leftover dirs whose origin matches a
            // known org repo that isn't a current member.
            let declared_keys: std::collections::BTreeSet<(String, String)> = members
                .iter()
                .map(|m| {
                    (
                        m.reference.org.as_str().to_string(),
                        m.reference.name.as_str().to_string(),
                    )
                })
                .collect();
            let mut new_matched: Vec<(String, crate::v5::config::RepoRef)> = Vec::new();
            for (dname, origin) in &dir_origin {
                if consumed_dirs.contains(dname) {
                    continue;
                }
                let Some(url) = origin else { continue };
                let Some(refs) = url_to_refs.get(url) else { continue };
                for (o, n) in refs {
                    if !declared_keys.contains(&(o.clone(), n.clone())) {
                        new_matched.push((
                            dname.clone(),
                            crate::v5::config::RepoRef {
                                org: o.clone().into(),
                                name: n.clone().into(),
                            },
                        ));
                        break;
                    }
                }
            }
            for d in &decisions {
                let reference = Some(members[d.idx].reference.clone());
                yield WorkspacesEvent::ReconcileEvent {
                    kind: d.kind.clone(),
                    reference,
                    dir: d.dir.clone(),
                    detail: None,
                };
            }
            for (dname, rref) in &new_matched {
                yield WorkspacesEvent::ReconcileEvent {
                    kind: "new_matched".into(),
                    reference: Some(rref.clone()),
                    dir: Some(dname.clone()),
                    detail: None,
                };
            }
            if !dry {
                let mut changed = false;
                let mut primary: std::collections::BTreeMap<usize, &MemberDecision> =
                    std::collections::BTreeMap::new();
                for d in &decisions {
                    if d.kind == "ambiguous" {
                        continue;
                    }
                    primary.entry(d.idx).or_insert(d);
                }
                let mut new_repos: Vec<WorkspaceRepo> = Vec::with_capacity(cfg.repos.len());
                for (mi, entry) in cfg.repos.iter().enumerate() {
                    let Some(d) = primary.get(&mi) else {
                        new_repos.push(entry.clone());
                        continue;
                    };
                    match d.kind.as_str() {
                        "matched" => new_repos.push(entry.clone()),
                        "renamed" => {
                            let reference = members[mi].reference.clone();
                            let dir = d.dir.clone().unwrap_or_default();
                            new_repos.push(WorkspaceRepo::Object { reference, dir });
                            changed = true;
                        }
                        "removed" => {
                            changed = true;
                        }
                        _ => new_repos.push(entry.clone()),
                    }
                }
                // V5LIFECYCLE-10: scan for .hyperforge/config.toml drift.
                // For each scanned dir, read the file (if any) and
                // compare its declared identity against the member
                // this dir resolved to. Informational only — no yaml
                // mutation happens from this check.
                for (dname, dpath) in &entries {
                    if let Ok(Some(hf_cfg)) = crate::v5::ops::fs::read_hyperforge_config(dpath) {
                        // Find the decision that bound this dir (if any).
                        let bound = decisions.iter().find(|d| d.dir.as_deref() == Some(dname));
                        if let Some(dec) = bound {
                            let m = &members[dec.idx];
                            // HYPE-5: owner half compares canonical identities,
                            // so an aliased owner (declared user vs workspace
                            // org, or vice-versa) is NOT reported as drift; only
                            // a genuine owner or name divergence is.
                            let owners_agree =
                                global.same_owner(hf_cfg.org.as_str(), m.reference.org.as_str());
                            let names_agree = hf_cfg.repo_name == m.reference.name.as_str();
                            if !(owners_agree && names_agree) {
                                yield WorkspacesEvent::ConfigDrift {
                                    dir: dname.clone(),
                                    declared_org: hf_cfg.org.as_str().to_string(),
                                    declared_repo: hf_cfg.repo_name,
                                    workspace_org: m.reference.org.as_str().to_string(),
                                    workspace_repo: m.reference.name.as_str().to_string(),
                                };
                            }
                        }
                    }
                }
                if changed {
                    cfg.repos = new_repos;
                    if let Err(e) = crate::v5::config::save_workspace(&ws_dir, &cfg) {
                        yield WorkspacesEvent::Error {
                            code: Some("io_error".into()),
                            message: e.to_string(),
                        };
                        return;
                    }
                }
            }
        }
    }

    /// V5WS-9: orchestrate per-member sync, aggregate a
    /// `WorkspaceSyncReport`. Serial execution; failures per member
    /// are counted and continue (D6). Read-only against forges, org
    /// yamls, workspace yamls, and the on-disk workspace `path`.
    #[plexus_macros::method(params(
        name = "Workspace name",
        include_dismissed = "Include members with lifecycle: dismissed (default false)"
    ))]
    pub async fn sync(
        &self,
        name: String,
        include_dismissed: Option<serde_json::Value>,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        let include_dismissed_flag = include_dismissed
            .as_ref()
            .is_some_and(|v| match v {
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::String(s) => matches!(s.as_str(), "true" | "1" | "yes"),
                _ => false,
            });
        async_stream::stream! {
            if name.is_empty() {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: "missing required parameter 'name'".into(),
                };
                return;
            }
            // Load workspace yaml. Missing workspaces dir is treated
            // as zero-workspace state, not a config error.
            let ws_dir = config_dir.join("workspaces");
            let all_ws = if ws_dir.is_dir() {
                match crate::v5::config::load_workspaces(&ws_dir) {
                    Ok(w) => w,
                    Err(e) => {
                        yield WorkspacesEvent::Error {
                            code: Some("config_error".into()),
                            message: e.to_string(),
                        };
                        return;
                    }
                }
            } else {
                std::collections::BTreeMap::new()
            };
            let Some(ws) = all_ws.iter().find(|w| w.1.name.as_str() == name).map(|(_k, v)| v) else {
                yield WorkspacesEvent::Error {
                    code: Some("not_found".into()),
                    message: format!("workspace '{name}' not found"),
                };
                return;
            };
            // Load orgs for repo lookup + credential resolution.
            let loaded = match crate::v5::config::load_all(&config_dir) {
                Ok(l) => l,
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let resolver = crate::v5::secrets::YamlSecretStore::new(&config_dir);
            let mut in_sync = 0u32;
            let mut drifted = 0u32;
            let mut errored = 0u32;
            let mut created = 0u32;
            let mut skipped = 0u32;
            let mut per_repo: Vec<serde_json::Value> = Vec::new();
            let total = u32::try_from(ws.repos.len()).unwrap_or(u32::MAX);

            for entry in &ws.repos {
                let (org_s, name_s) = match entry {
                    crate::v5::config::WorkspaceRepo::Shorthand(s) => {
                        match s.split_once('/') {
                            Some((o, n)) => (o.to_string(), n.to_string()),
                            None => {
                                let wire = crate::v5::repos::RepoRefWire {
                                    org: String::new(),
                                    name: s.clone(),
                                };
                                let diff = WorkspacesEvent::SyncDiff {
                                    reference: wire.clone(),
                                    url: None,
                                    status: "errored".into(),
                                    drift: vec![],
                                    error_class: Some("validation".into()),
                                    remote: None,
                                };
                                per_repo.push(serde_json::to_value(&diff).unwrap_or(serde_json::Value::Null));
                                errored += 1;
                                yield diff;
                                continue;
                            }
                        }
                    }
                    crate::v5::config::WorkspaceRepo::Object { reference, .. } => (
                        reference.org.as_str().to_string(),
                        reference.name.as_str().to_string(),
                    ),
                };
                let wire = crate::v5::repos::RepoRefWire {
                    org: org_s.clone(),
                    name: name_s.clone(),
                };
                // Find the org.
                let Some(org_cfg) = loaded.orgs.get(&crate::v5::config::OrgName::from(org_s.as_str())) else {
                    let diff = WorkspacesEvent::SyncDiff {
                        reference: wire.clone(),
                        url: None,
                        status: "errored".into(),
                        drift: vec![],
                        error_class: Some("not_found".into()),
                        remote: None,
                    };
                    per_repo.push(serde_json::to_value(&diff).unwrap_or(serde_json::Value::Null));
                    errored += 1;
                    yield diff;
                    continue;
                };
                // Find the repo.
                let Some(repo) = org_cfg.repos.iter().find(|r| r.name.as_str() == name_s) else {
                    let diff = WorkspacesEvent::SyncDiff {
                        reference: wire.clone(),
                        url: None,
                        status: "errored".into(),
                        drift: vec![],
                        error_class: Some("not_found".into()),
                        remote: None,
                    };
                    per_repo.push(serde_json::to_value(&diff).unwrap_or(serde_json::Value::Null));
                    errored += 1;
                    yield diff;
                    continue;
                };
                // V5LIFECYCLE-10: skip dismissed members unless include_dismissed.
                if !include_dismissed_flag
                    && repo.metadata.as_ref().map_or(crate::v5::config::RepoLifecycle::Active, |m| m.lifecycle)
                        == crate::v5::config::RepoLifecycle::Dismissed
                {
                    let skip = WorkspacesEvent::SyncSkipped {
                        reference: wire.clone(),
                        reason: "dismissed".into(),
                    };
                    per_repo.push(serde_json::to_value(&skip).unwrap_or(serde_json::Value::Null));
                    skipped += 1;
                    yield skip;
                    continue;
                }
                // Take the first in-scope remote as the canonical
                // sync target (V5PARITY-34: per-repo `forges` filter).
                // (per-member single SyncDiff per the ticket).
                let Some(remote) = crate::v5::ops::repo::canonical_remote_in_scope(
                    repo, &loaded.global.provider_map,
                ) else {
                    let diff = WorkspacesEvent::SyncDiff {
                        reference: wire.clone(),
                        url: None,
                        status: "errored".into(),
                        drift: vec![],
                        error_class: Some("validation".into()),
                        remote: None,
                    };
                    per_repo.push(serde_json::to_value(&diff).unwrap_or(serde_json::Value::Null));
                    errored += 1;
                    yield diff;
                    continue;
                };
                // Resolve provider + credentials + call adapter.
                let provider = match crate::v5::repos::derive_provider(remote, &loaded.global.provider_map) {
                    Ok(p) => p,
                    Err(e) => {
                        let diff = WorkspacesEvent::SyncDiff {
                            reference: wire.clone(),
                            url: Some(remote.url.as_str().to_string()),
                            status: "errored".into(),
                            drift: vec![],
                            error_class: Some("network".into()),
                            remote: None,
                        };
                        per_repo.push(serde_json::to_value(&diff).unwrap_or(serde_json::Value::Null));
                        errored += 1;
                        yield diff;
                        let _ = e;
                        continue;
                    }
                };
                let token_ref = org_cfg
                    .forge
                    .credentials
                    .iter()
                    .find(|c| matches!(c.cred_type, crate::v5::config::CredentialType::Token))
                    .map(|c| c.key.clone());
                let token_ref_str = token_ref.as_deref();
                let fallback_token_ref = Some(crate::v5::ops::repo::default_token_ref_for(org_cfg));
                let _ = provider; // provider derivation happens inside ops::repo::*
                let repo_ref = crate::v5::config::RepoRef {
                    org: crate::v5::config::OrgName::from(org_s.as_str()),
                    name: crate::v5::config::RepoName::from(name_s.as_str()),
                };
                // V5LIFECYCLE-4: probe for existence via ops::repo.
                match crate::v5::ops::repo::exists_on_forge(
                    remote, &repo_ref, &loaded.global.provider_map, &resolver, token_ref_str, fallback_token_ref.clone(),
                ).await {
                    Ok(false) => {
                        // Absent → create via ops::repo::create_on_forge.
                        let vis = repo
                            .metadata
                            .as_ref()
                            .and_then(|m| m.visibility.as_deref())
                            .map(|s| match s {
                                "public" => crate::v5::adapters::ProviderVisibility::Public,
                                "internal" => crate::v5::adapters::ProviderVisibility::Internal,
                                _ => crate::v5::adapters::ProviderVisibility::Private,
                            })
                            .unwrap_or(crate::v5::adapters::ProviderVisibility::Private);
                        let desc = repo
                            .metadata
                            .as_ref()
                            .and_then(|m| m.description.clone())
                            .unwrap_or_default();
                        let diff = match crate::v5::ops::repo::create_on_forge(
                            remote, &repo_ref, vis, &desc, &loaded.global.provider_map, &resolver, token_ref_str, fallback_token_ref.clone(),
                        ).await {
                            Ok(()) => {
                                created += 1;
                                WorkspacesEvent::SyncDiff {
                                    reference: wire.clone(),
                                    url: Some(remote.url.as_str().to_string()),
                                    status: "created".into(),
                                    drift: vec![],
                                    error_class: None,
                                    remote: None,
                                }
                            }
                            Err(e) => {
                                errored += 1;
                                WorkspacesEvent::SyncDiff {
                                    reference: wire.clone(),
                                    url: Some(remote.url.as_str().to_string()),
                                    status: "errored".into(),
                                    drift: vec![],
                                    error_class: Some(e.class.as_str().to_string()),
                                    remote: None,
                                }
                            }
                        };
                        per_repo.push(serde_json::to_value(&diff).unwrap_or(serde_json::Value::Null));
                        yield diff;
                        continue;
                    }
                    Err(e) => {
                        errored += 1;
                        let diff = WorkspacesEvent::SyncDiff {
                            reference: wire.clone(),
                            url: Some(remote.url.as_str().to_string()),
                            status: "errored".into(),
                            drift: vec![],
                            error_class: Some(e.class.as_str().to_string()),
                            remote: None,
                        };
                        per_repo.push(serde_json::to_value(&diff).unwrap_or(serde_json::Value::Null));
                        yield diff;
                        continue;
                    }
                    Ok(true) => {} // proceed to sync
                }

                // V5LIFECYCLE-3: delegate the per-remote sync to the
                // single primitive. We filter to just this member's
                // first remote (V5WS-9's "one SyncDiff per member"
                // contract) by passing its url as the filter.
                let outcomes = crate::v5::ops::repo::sync_one(
                    repo,
                    org_cfg,
                    &loaded.global.provider_map,
                    &resolver,
                    Some(remote.url.as_str()),
                ).await;
                let diff = if let Some(o) = outcomes.into_iter().next() {
                    match o.status {
                        crate::v5::ops::repo::SyncStatus::InSync => { in_sync += 1; }
                        crate::v5::ops::repo::SyncStatus::Drifted => { drifted += 1; }
                        crate::v5::ops::repo::SyncStatus::Errored => { errored += 1; }
                    }
                    WorkspacesEvent::SyncDiff {
                        reference: wire.clone(),
                        url: Some(o.remote.url.as_str().to_string()),
                        status: o.status.as_str().to_string(),
                        drift: o.drift.into_iter().map(|d| crate::v5::repos::DriftField {
                            field: d.field,
                            local: d.local,
                            remote: d.remote,
                        }).collect(),
                        error_class: o.error_class.map(|e| e.as_str().to_string()),
                        remote: o.metadata,
                    }
                } else {
                    errored += 1;
                    WorkspacesEvent::SyncDiff {
                        reference: wire.clone(),
                        url: Some(remote.url.as_str().to_string()),
                        status: "errored".into(),
                        drift: vec![],
                        error_class: Some("network".into()),
                        remote: None,
                    }
                };
                per_repo.push(serde_json::to_value(&diff).unwrap_or(serde_json::Value::Null));
                yield diff;
            }

            yield WorkspacesEvent::WorkspaceSyncReport {
                name: ws.name.as_str().to_string(),
                total,
                in_sync,
                drifted,
                errored,
                created,
                skipped,
                per_repo,
            };
        }
    }

    // ==================================================================
    // V5PARITY-2: workspaces.discover — FS scan → workspace yaml.
    // ==================================================================

    #[plexus_macros::method(params(
        path = "Directory to scan for git checkouts",
        name = "Workspace name (defaults to path basename)",
        dry_run = "Preview without writing"
    ))]
    pub async fn discover(
        &self,
        path: String,
        name: Option<String>,
        dry_run: Option<serde_json::Value>,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            if path.is_empty() {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: "missing required parameter 'path'".into(),
                };
                return;
            }
            let dry = dry_run.as_ref().is_some_and(|v| match v {
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::String(s) => matches!(s.as_str(), "true" | "1" | "yes"),
                _ => false,
            });
            let scan_root = std::path::PathBuf::from(&path);
            if !scan_root.is_dir() {
                yield WorkspacesEvent::Error {
                    code: Some("not_a_directory".into()),
                    message: format!("path is not a directory: {path}"),
                };
                return;
            }
            // Load every org so we can match origin URLs → <org>/<name>.
            let orgs_dir = config_dir.join("orgs");
            let orgs = if orgs_dir.is_dir() {
                match crate::v5::ops::state::load_orgs(&orgs_dir) {
                    Ok(o) => o,
                    Err(e) => {
                        yield WorkspacesEvent::Error {
                            code: Some("config_error".into()),
                            message: e.to_string(),
                        };
                        return;
                    }
                }
            } else {
                std::collections::BTreeMap::new()
            };
            // Build URL → (org, repo_name) lookup.
            let mut url_index: std::collections::BTreeMap<String, (String, String)> =
                std::collections::BTreeMap::new();
            for (_, org) in &orgs {
                for r in &org.repos {
                    for rem in &r.remotes {
                        url_index.insert(
                            rem.url.as_str().to_string(),
                            (org.name.as_str().to_string(), r.name.as_str().to_string()),
                        );
                    }
                }
            }
            // Walk the path.
            let mut entries: Vec<(String, std::path::PathBuf)> = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&scan_root) {
                for ent in rd.flatten() {
                    let p = ent.path();
                    if !p.is_dir() { continue; }
                    let Some(nm) = p.file_name().and_then(|s| s.to_str()) else { continue };
                    entries.push((nm.to_string(), p));
                }
            }
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            // Also load existing workspace yamls to detect already_member.
            let ws_dir = config_dir.join("workspaces");
            let existing_ws = if ws_dir.is_dir() {
                crate::v5::ops::state::load_workspaces(&ws_dir).unwrap_or_default()
            } else {
                std::collections::BTreeMap::new()
            };
            let mut membership: std::collections::BTreeSet<(String, String)> =
                std::collections::BTreeSet::new();
            for (_, ws) in &existing_ws {
                for entry in &ws.repos {
                    let key = match entry {
                        crate::v5::config::WorkspaceRepo::Shorthand(s) => s.split_once('/').map(|(o, n)| (o.to_string(), n.to_string())),
                        crate::v5::config::WorkspaceRepo::Object { reference, .. } => Some((
                            reference.org.as_str().to_string(),
                            reference.name.as_str().to_string(),
                        )),
                    };
                    if let Some(k) = key {
                        membership.insert(k);
                    }
                }
            }
            // For each dir, resolve origin + classify.
            let mut matched_refs: Vec<(String, String)> = Vec::new();
            for (dname, dpath) in &entries {
                let origin = read_git_origin(dpath);
                match origin.as_ref().and_then(|u| url_index.get(u)) {
                    Some((o, n)) => {
                        let already = membership.contains(&(o.clone(), n.clone()));
                        yield WorkspacesEvent::DiscoverMatch {
                            dir: dname.clone(),
                            status: if already { "already_member".into() } else { "matched".into() },
                            reference: Some(crate::v5::repos::RepoRefWire {
                                org: o.clone(),
                                name: n.clone(),
                            }),
                            origin: origin.clone(),
                        };
                        if !already { matched_refs.push((o.clone(), n.clone())); }
                    }
                    None => {
                        yield WorkspacesEvent::DiscoverMatch {
                            dir: dname.clone(),
                            status: "orphan".into(),
                            reference: None,
                            origin,
                        };
                    }
                }
            }
            // Build a workspace yaml if anything matched + not dry.
            let ws_name = name.unwrap_or_else(|| {
                scan_root.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("discovered")
                    .to_string()
            });
            if !matched_refs.is_empty() && !dry {
                let repos: Vec<crate::v5::config::WorkspaceRepo> = matched_refs.iter()
                    .map(|(o, n)| crate::v5::config::WorkspaceRepo::Shorthand(format!("{o}/{n}")))
                    .collect();
                let new_cfg = crate::v5::config::WorkspaceConfig {
                    name: crate::v5::config::WorkspaceName::from(ws_name.as_str()),
                    path: crate::v5::config::FsPath::from(path.as_str()),
                    repos,
                };
                let ws_dir = config_dir.join("workspaces");
                if let Err(e) = crate::v5::ops::state::save_workspace(&ws_dir, &new_cfg) {
                    yield WorkspacesEvent::Error {
                        code: Some("io_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            }
            yield WorkspacesEvent::WorkspaceDiscovered {
                name: ws_name,
                path: path.clone(),
                repo_count: u32::try_from(matched_refs.len()).unwrap_or(u32::MAX),
            };
        }
    }

    // ==================================================================
    // V5PARITY-3 / V5PARITY-14: workspace git verbs.
    //
    // Sequential v1 (matches V5PARITY-3 scope; bounded parallelism is a
    // separate concern not yet ticketed). Each public method has its
    // own loop and yields its own events directly — no central
    // string-dispatch helper. The shared shape (load workspace + decode
    // members) lives in the small typed helpers `load_iter_ctx` and
    // `member_ctxs` defined below.
    // ==================================================================

    #[plexus_macros::method(params(name = "Workspace name"))]
    pub async fn clone(
        &self,
        name: String,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            let (ws, loaded) = match load_iter_ctx(&config_dir, &name).await {
                Ok(t) => t, Err(e) => { yield e; return; }
            };
            let ws_path = std::path::PathBuf::from(ws.path.as_str());
            let total = u32::try_from(ws.repos.len()).unwrap_or(u32::MAX);
            let mut ok = 0u32; let mut errored = 0u32;
            for ctx in member_ctxs(&ws, &loaded, &ws_path) {
                let url = ctx.repo.and_then(|r| r.canonical_remote()).map(|r| r.url.as_str().to_string());
                let dir = ctx.dir.clone();
                // HYPERFORGE-64 (2): git off the runtime, so the socket keeps answering.
                let result = crate::v5::ops::git::off_runtime(move || {
                    url.ok_or_else(|| "no remotes".to_string())
                        .and_then(|u| crate::v5::ops::git::clone_repo(&u, &dir).map_err(|e| e.to_string()))
                }).await;
                match result {
                    Ok(()) => { ok += 1; yield member_ok(&ctx.reference, "clone"); }
                    Err(e) => { errored += 1; yield member_err(&ctx.reference, "clone", e); }
                }
            }
            yield WorkspacesEvent::WorkspaceGitSummary {
                name: ws.name.as_str().to_string(), op: "clone".into(),
                total, ok, errored, not_members: Vec::new(), note: None,
            };
        }
    }

    #[plexus_macros::method(params(name = "Workspace name"))]
    pub async fn fetch(
        &self,
        name: String,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            let (ws, loaded) = match load_iter_ctx(&config_dir, &name).await {
                Ok(t) => t, Err(e) => { yield e; return; }
            };
            let ws_path = std::path::PathBuf::from(ws.path.as_str());
            let total = u32::try_from(ws.repos.len()).unwrap_or(u32::MAX);
            let mut ok = 0u32; let mut errored = 0u32;
            for ctx in member_ctxs(&ws, &loaded, &ws_path) {
                let dir = ctx.dir.clone();
                // HYPERFORGE-64 (2): git off the runtime, so the socket keeps answering.
                match crate::v5::ops::git::off_runtime(move || crate::v5::ops::git::fetch(&dir, None)).await {
                    Ok(()) => { ok += 1; yield member_ok(&ctx.reference, "fetch"); }
                    Err(e) => { errored += 1; yield member_err(&ctx.reference, "fetch", e.to_string()); }
                }
            }
            yield WorkspacesEvent::WorkspaceGitSummary {
                name: ws.name.as_str().to_string(), op: "fetch".into(),
                total, ok, errored, not_members: Vec::new(), note: None,
            };
        }
    }

    #[plexus_macros::method(params(name = "Workspace name"))]
    pub async fn pull(
        &self,
        name: String,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            let (ws, loaded) = match load_iter_ctx(&config_dir, &name).await {
                Ok(t) => t, Err(e) => { yield e; return; }
            };
            let ws_path = std::path::PathBuf::from(ws.path.as_str());
            let total = u32::try_from(ws.repos.len()).unwrap_or(u32::MAX);
            let mut ok = 0u32; let mut errored = 0u32;
            let mut member_dirs = Vec::new();
            for ctx in member_ctxs(&ws, &loaded, &ws_path) {
                member_dirs.push(ctx.dir.clone());
                let dir = ctx.dir.clone();
                // HYPERFORGE-64 (2): each member's git runs off the async
                // runtime. Inline, a pull over many members held one worker
                // for the whole run and the socket stopped answering pings.
                let pulled = crate::v5::ops::git::off_runtime(move || {
                    let branch = crate::v5::ops::git::status(&dir).ok()
                        .and_then(|s| s.branch).unwrap_or_else(|| "main".into());
                    crate::v5::ops::git::pull_ff(&dir, "origin", &branch)
                }).await;
                match pulled {
                    Ok(()) => { ok += 1; yield member_ok(&ctx.reference, "pull"); }
                    Err(e) => { errored += 1; yield member_err(&ctx.reference, "pull", e.to_string()); }
                }
            }
            // HYPERFORGE-64 (3): a clone under the path that is not a member
            // was skipped without a word; name it.
            let not_members = checkouts_not_members(&ws_path, &member_dirs);
            let note = stray_note(&ws_path, &not_members);
            yield WorkspacesEvent::WorkspaceGitSummary {
                name: ws.name.as_str().to_string(), op: "pull".into(),
                total, ok, errored, not_members, note,
            };
        }
    }

    #[plexus_macros::method(params(name = "Workspace name"))]
    pub async fn push(
        &self,
        name: String,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            let (ws, loaded) = match load_iter_ctx(&config_dir, &name).await {
                Ok(t) => t, Err(e) => { yield e; return; }
            };
            let ws_path = std::path::PathBuf::from(ws.path.as_str());
            let total = u32::try_from(ws.repos.len()).unwrap_or(u32::MAX);
            let mut ok = 0u32; let mut errored = 0u32;
            for ctx in member_ctxs(&ws, &loaded, &ws_path) {
                let dir = ctx.dir.clone();
                // HYPERFORGE-64 (2): git off the runtime, so the socket keeps answering.
                match crate::v5::ops::git::off_runtime(move || crate::v5::ops::git::push_refs(&dir, "origin", None)).await {
                    Ok(()) => { ok += 1; yield member_ok(&ctx.reference, "push"); }
                    Err(e) => { errored += 1; yield member_err(&ctx.reference, "push", e.to_string()); }
                }
            }
            yield WorkspacesEvent::WorkspaceGitSummary {
                name: ws.name.as_str().to_string(), op: "push".into(),
                total, ok, errored, not_members: Vec::new(), note: None,
            };
        }
    }

    #[plexus_macros::method(params(name = "Workspace name"))]
    pub async fn status(
        &self,
        name: String,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            let (ws, loaded) = match load_iter_ctx(&config_dir, &name).await {
                Ok(t) => t, Err(e) => { yield e; return; }
            };
            let ws_path = std::path::PathBuf::from(ws.path.as_str());
            let total = u32::try_from(ws.repos.len()).unwrap_or(u32::MAX);
            let (mut clean, mut dirty, mut ahead, mut behind, mut errored) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for ctx in member_ctxs(&ws, &loaded, &ws_path) {
                match crate::v5::ops::git::status(&ctx.dir) {
                    Ok(s) => {
                        let is_dirty = s.dirty();
                        if is_dirty { dirty += 1; } else { clean += 1; }
                        if s.ahead > 0 { ahead += 1; }
                        if s.behind > 0 { behind += 1; }
                        yield WorkspacesEvent::StatusSnapshot {
                            reference: ctx.reference,
                            branch: s.branch, upstream: s.upstream,
                            ahead: s.ahead, behind: s.behind,
                            staged: s.staged, unstaged: s.unstaged, untracked: s.untracked,
                            dirty: is_dirty,
                        };
                    }
                    Err(e) => {
                        errored += 1;
                        yield member_err(&ctx.reference, "status", e.to_string());
                    }
                }
            }
            yield WorkspacesEvent::WorkspaceStatusSummary {
                name: ws.name.as_str().to_string(),
                total, clean, dirty, ahead, behind, errored,
            };
        }
    }

    #[plexus_macros::method(params(
        name = "Workspace name",
        branch = "Branch name",
        create = "If true, create the branch; a branch that exists is refused, never reset"
    ))]
    pub async fn checkout(
        &self,
        name: String,
        branch: String,
        create: Option<serde_json::Value>,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            if branch.is_empty() {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: "missing required parameter 'branch'".into(),
                };
                return;
            }
            let create_b = create.as_ref().is_some_and(to_bool);
            let (ws, loaded) = match load_iter_ctx(&config_dir, &name).await {
                Ok(t) => t, Err(e) => { yield e; return; }
            };
            let ws_path = std::path::PathBuf::from(ws.path.as_str());
            let total = u32::try_from(ws.repos.len()).unwrap_or(u32::MAX);
            let mut ok = 0u32; let mut errored = 0u32;
            for ctx in member_ctxs(&ws, &loaded, &ws_path) {
                match crate::v5::ops::git::checkout(&ctx.dir, &branch, create_b) {
                    Ok(()) => { ok += 1; yield member_ok(&ctx.reference, "checkout"); }
                    Err(e) => { errored += 1; yield member_err(&ctx.reference, "checkout", e.to_string()); }
                }
            }
            yield WorkspacesEvent::WorkspaceGitSummary {
                name: ws.name.as_str().to_string(), op: "checkout".into(),
                total, ok, errored, not_members: Vec::new(), note: None,
            };
        }
    }

    #[plexus_macros::method(params(
        name = "Workspace name",
        ticket = "Ticket id: the branch name and the worktrees' folder, `wt-<ticket>`",
        repos = "Member repo names to include",
        base = "Git ref a new branch starts from (default each repo's HEAD)",
        remove = "Remove these worktrees instead; the branches stay (default false)",
        dry_run = "Say what would happen; write nothing (default false)"
    ))]
    pub async fn worktree(
        &self,
        name: String,
        ticket: String,
        repos: Vec<String>,
        base: Option<String>,
        remove: Option<bool>,
        dry_run: Option<bool>,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            if !valid_ticket(&ticket) {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: format!("'{ticket}' is not a ticket id: letters, digits, `-`, `_`, `.`, not starting with `-` or `.`"),
                };
                return;
            }
            let (ws, loaded) = match load_iter_ctx(&config_dir, &name).await {
                Ok(t) => t, Err(e) => { yield e; return; }
            };
            let ws_path = std::path::PathBuf::from(ws.path.as_str());
            let members: Vec<_> = member_ctxs(&ws, &loaded, &ws_path).collect();
            let missing: Vec<&String> = repos.iter().filter(|r| !members.iter().any(|m| &m.reference.name == *r)).collect();
            if repos.is_empty() || !missing.is_empty() {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: if repos.is_empty() { "name at least one repo".into() } else { format!("not members of {name}: {missing:?}") },
                };
                return;
            }
            let root = ws_path.join(format!("wt-{ticket}"));
            let (dry, removing) = (dry_run.unwrap_or(false), remove.unwrap_or(false));
            let op = if removing { "worktree_remove" } else { "worktree" };
            let (mut ok, mut errored) = (0u32, 0u32);
            for ctx in members.iter().filter(|m| repos.contains(&m.reference.name)) {
                let dest = root.join(&ctx.reference.name);
                let done = if dry {
                    Ok(())
                } else if removing {
                    crate::v5::ops::git::worktree_remove(&ctx.dir, &dest)
                } else if dest.exists() {
                    Err(format!("{} exists", dest.display()))
                } else {
                    crate::v5::ops::git::worktree_add(&ctx.dir, &dest, &ticket, base.as_deref())
                };
                let (status, message) = match done {
                    Ok(()) => { ok += 1; (if dry { "would" } else { "ok" }, dest.display().to_string()) }
                    Err(e) => { errored += 1; ("errored", e) }
                };
                yield WorkspacesEvent::MemberGitResult {
                    reference: ctx.reference.clone(),
                    op: op.into(),
                    status: status.into(),
                    message: Some(message),
                };
            }
            if removing && !dry {
                // Only an empty folder goes; anything left in it stays.
                let _ = std::fs::remove_dir(&root);
            }
            yield WorkspacesEvent::WorkspaceGitSummary {
                name: ws.name.as_str().to_string(), op: op.into(),
                total: u32::try_from(repos.len()).unwrap_or(u32::MAX), ok, errored, not_members: Vec::new(), note: None,
            };
        }
    }

    #[plexus_macros::method(params(
        name = "Workspace name",
        dirty_only = "Only checkouts with uncommitted changes or commits unpushed or unpulled, or with no upstream (default false)",
        table = "One aligned text table in place of a row per checkout (default false)"
    ))]
    pub async fn orient(
        &self,
        name: String,
        dirty_only: Option<bool>,
        table: Option<bool>,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            let (ws, loaded) = match load_iter_ctx(&config_dir, &name).await {
                Ok(t) => t, Err(e) => { yield e; return; }
            };
            let ws_path = std::path::PathBuf::from(ws.path.as_str());
            let only = dirty_only.unwrap_or(false);
            let mut rows: Vec<OrientRow> = Vec::new();
            for ctx in member_ctxs(&ws, &loaded, &ws_path) {
                if !ctx.dir.exists() {
                    continue;
                }
                let checkouts = crate::v5::ops::git::worktree_list(&ctx.dir).unwrap_or_else(|_| vec![ctx.dir.clone()]);
                for (i, path) in checkouts.into_iter().enumerate() {
                    if !path.exists() {
                        continue;
                    }
                    let row = match crate::v5::ops::git::status(&path) {
                        Ok(s) => OrientRow {
                            repo: ctx.reference.name.clone(),
                            path: path.display().to_string(),
                            worktree: i > 0,
                            dirty: s.staged + s.unstaged + s.untracked,
                            branch: s.branch,
                            upstream: s.upstream,
                            ahead: s.ahead,
                            behind: s.behind,
                            error: None,
                        },
                        Err(e) => OrientRow {
                            repo: ctx.reference.name.clone(),
                            path: path.display().to_string(),
                            worktree: i > 0,
                            branch: None, upstream: None, ahead: 0, behind: 0, dirty: 0,
                            error: Some(e.to_string()),
                        },
                    };
                    if !only || row.needs_attention() {
                        rows.push(row);
                    }
                }
            }
            if table.unwrap_or(false) {
                yield WorkspacesEvent::OrientTable { text: orient_table(&rows) };
            } else {
                for row in rows {
                    yield WorkspacesEvent::OrientCheckout { row };
                }
            }
        }
    }

    #[plexus_macros::method(params(
        name = "Workspace name",
        message = "Commit message",
        only_dirty = "If true (default), skip members with no staged changes",
        allow_empty = "If true, pass --allow-empty to git commit"
    ))]
    pub async fn commit(
        &self,
        name: String,
        message: String,
        only_dirty: Option<serde_json::Value>,
        allow_empty: Option<serde_json::Value>,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            if message.is_empty() {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: "missing required parameter 'message'".into(),
                };
                return;
            }
            let only_dirty_b = only_dirty.as_ref().map_or(true, to_bool);
            let allow_empty_b = allow_empty.as_ref().is_some_and(to_bool);
            let (ws, loaded) = match load_iter_ctx(&config_dir, &name).await {
                Ok(t) => t, Err(e) => { yield e; return; }
            };
            let ws_path = std::path::PathBuf::from(ws.path.as_str());
            let total = u32::try_from(ws.repos.len()).unwrap_or(u32::MAX);
            let mut ok = 0u32; let mut errored = 0u32; let mut skipped = 0u32;
            for ctx in member_ctxs(&ws, &loaded, &ws_path) {
                if only_dirty_b && !allow_empty_b {
                    let nothing_to_commit = crate::v5::ops::git::status(&ctx.dir)
                        .map(|s| s.staged == 0)
                        .unwrap_or(false);
                    if nothing_to_commit {
                        skipped += 1;
                        yield WorkspacesEvent::MemberGitResult {
                            reference: ctx.reference,
                            op: "commit".into(), status: "skipped".into(),
                            message: Some("no staged changes".into()),
                        };
                        continue;
                    }
                }
                match crate::v5::ops::git::commit_with(&ctx.dir, &message, allow_empty_b) {
                    Ok(()) => { ok += 1; yield member_ok(&ctx.reference, "commit"); }
                    Err(e) => { errored += 1; yield member_err(&ctx.reference, "commit", e.to_string()); }
                }
            }
            yield WorkspacesEvent::WorkspaceGitSummary {
                name: ws.name.as_str().to_string(), op: "commit".into(),
                total, ok: ok + skipped, errored, not_members: Vec::new(), note: None,
            };
        }
    }

    #[plexus_macros::method(params(
        name = "Workspace name",
        tag = "Tag name",
        message = "Optional annotated-tag message"
    ))]
    pub async fn tag(
        &self,
        name: String,
        tag: String,
        message: Option<String>,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            if tag.is_empty() {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: "missing required parameter 'tag'".into(),
                };
                return;
            }
            let (ws, loaded) = match load_iter_ctx(&config_dir, &name).await {
                Ok(t) => t, Err(e) => { yield e; return; }
            };
            let ws_path = std::path::PathBuf::from(ws.path.as_str());
            let total = u32::try_from(ws.repos.len()).unwrap_or(u32::MAX);
            let mut ok = 0u32; let mut errored = 0u32;
            for ctx in member_ctxs(&ws, &loaded, &ws_path) {
                let result = match message.as_deref() {
                    Some(m) if !m.is_empty() => crate::v5::ops::git::tag_annotated(&ctx.dir, &tag, m),
                    _ => crate::v5::ops::git::tag(&ctx.dir, &tag),
                };
                match result {
                    Ok(()) => { ok += 1; yield member_ok(&ctx.reference, "tag"); }
                    Err(e) => { errored += 1; yield member_err(&ctx.reference, "tag", e.to_string()); }
                }
            }
            yield WorkspacesEvent::WorkspaceGitSummary {
                name: ws.name.as_str().to_string(), op: "tag".into(),
                total, ok, errored, not_members: Vec::new(), note: None,
            };
        }
    }

    // ==================================================================
    // V5PARITY-22: workspaces.from_org — one-shot workspace creation.
    // ==================================================================

    #[plexus_macros::method(params(
        org = "Org name (must already be configured)",
        target_path = "Absolute filesystem path for the workspace (named target_path because synapse path-expands params named exactly 'path')",
        name = "Workspace name (defaults to org name)",
        filter = "Glob filter on member names; comma-separated for multiple",
        clone = "Clone every member into the path (default: true)",
        update = "On re-run, run pull on existing checkouts (default: false)"
    ))]
    pub async fn from_org(
        &self,
        org: String,
        target_path: String,
        name: Option<String>,
        filter: Option<String>,
        clone: Option<serde_json::Value>,
        update: Option<serde_json::Value>,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            if org.is_empty() || target_path.is_empty() {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: "missing required parameter 'org' or 'target_path'".into(),
                };
                return;
            }
            let do_clone = clone.as_ref().map_or(true, to_bool);
            let do_update = update.as_ref().is_some_and(to_bool);
            let ws_name = name.unwrap_or_else(|| org.clone());
            let ws_path = std::path::PathBuf::from(&target_path);

            // Load org config.
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l,
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let org_key = crate::v5::config::OrgName::from(org.as_str());
            let Some(org_cfg) = loaded.orgs.get(&org_key) else {
                yield WorkspacesEvent::Error {
                    code: Some("not_found".into()),
                    message: format!("org '{org}' not found"),
                };
                return;
            };
            // Filter member set.
            let glob = filter.as_deref().map(|s| GlobSet::from_csv(s)).transpose();
            let glob = match glob {
                Ok(g) => g,
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("validation".into()),
                        message: format!("invalid filter: {e}"),
                    };
                    return;
                }
            };
            let candidates: Vec<&crate::v5::config::OrgRepo> = org_cfg.repos.iter()
                .filter(|r| glob.as_ref().map_or(true, |g| g.matches(r.name.as_str())))
                .collect();
            if candidates.is_empty() {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: "filter matched zero repos".into(),
                };
                return;
            }
            // Ensure path exists.
            if let Err(e) = std::fs::create_dir_all(&ws_path) {
                yield WorkspacesEvent::Error {
                    code: Some("io_error".into()),
                    message: format!("create {}: {e}", ws_path.display()),
                };
                return;
            }
            // Load (or create) the workspace yaml.
            let ws_dir = config_dir.join("workspaces");
            let existing_ws = if ws_dir.is_dir() {
                crate::v5::ops::state::load_workspaces(&ws_dir).ok()
                    .and_then(|m| m.into_iter().find(|(_, w)| w.name.as_str() == ws_name)
                        .map(|(_, w)| w))
            } else {
                None
            };
            let mut ws = existing_ws.unwrap_or_else(|| crate::v5::config::WorkspaceConfig {
                name: crate::v5::config::WorkspaceName::from(ws_name.as_str()),
                path: crate::v5::config::FsPath::from(target_path.as_str()),
                repos: Vec::new(),
            });
            let was_new = ws.repos.is_empty();
            // Existing membership index.
            let mut already: std::collections::BTreeSet<(String, String)> = ws.repos.iter()
                .filter_map(|wr| match wr {
                    crate::v5::config::WorkspaceRepo::Shorthand(s) => s.split_once('/')
                        .map(|(o, n)| (o.to_string(), n.to_string())),
                    crate::v5::config::WorkspaceRepo::Object { reference, .. } => Some((
                        reference.org.as_str().to_string(),
                        reference.name.as_str().to_string(),
                    )),
                })
                .collect();
            if was_new {
                yield WorkspacesEvent::WorkspaceCreated {
                    name: ws_name.clone(),
                    path: target_path.clone(),
                    org: org.clone(),
                };
            }
            // Add members.
            for repo in &candidates {
                let key = (org.clone(), repo.name.as_str().to_string());
                let already_present = already.contains(&key);
                if !already_present {
                    ws.repos.push(crate::v5::config::WorkspaceRepo::Shorthand(
                        format!("{}/{}", key.0, key.1),
                    ));
                    already.insert(key.clone());
                }
                yield WorkspacesEvent::MemberAdded {
                    reference: crate::v5::repos::RepoRefWire {
                        org: key.0.clone(), name: key.1.clone(),
                    },
                    already_present,
                };
            }
            // Persist workspace yaml.
            if let Err(e) = crate::v5::ops::state::save_workspace(&ws_dir, &ws) {
                yield WorkspacesEvent::Error {
                    code: Some("io_error".into()),
                    message: e.to_string(),
                };
                return;
            }
            // Optionally clone (or pull-update) each member.
            if !do_clone {
                yield WorkspacesEvent::WorkspaceGitSummary {
                    name: ws_name, op: "from_org".into(),
                    total: u32::try_from(candidates.len()).unwrap_or(u32::MAX),
                    ok: 0, errored: 0, not_members: Vec::new(), note: None,
                };
                return;
            }
            let total = u32::try_from(candidates.len()).unwrap_or(u32::MAX);
            let mut ok = 0u32;
            let mut errored = 0u32;
            // Resolve org's SSH key for clone forwarding (V5PARITY-5).
            let key_path = ssh_key_path_for_org(org_cfg);
            let ssh_cmd = key_path.as_ref().map(|p| crate::v5::ops::git::format_ssh_command(p));
            for repo in &candidates {
                let wire = crate::v5::repos::RepoRefWire {
                    org: org.clone(),
                    name: repo.name.as_str().to_string(),
                };
                let dir = ws_path.join(repo.name.as_str());
                let url = repo.canonical_remote().map(|r| r.url.as_str().to_string());
                let (ssh_cmd, key_path) = (ssh_cmd.clone(), key_path.clone());
                // HYPERFORGE-64 (2): git off the runtime, so the socket keeps answering.
                let result: Result<(), String> = crate::v5::ops::git::off_runtime(move || {
                    if dir.exists() {
                        if !do_update { return Ok(()); }
                        // pull on existing
                        let branch = crate::v5::ops::git::status(&dir).ok()
                            .and_then(|s| s.branch).unwrap_or_else(|| "main".into());
                        crate::v5::ops::git::pull_ff(&dir, "origin", &branch)
                            .map_err(|e| e.to_string())
                    } else {
                        let url = url.as_deref().ok_or_else(|| "no remote".to_string())?;
                        let env: Vec<(&str, &str)> = match ssh_cmd.as_deref() {
                            Some(s) => vec![("GIT_SSH_COMMAND", s)],
                            None => Vec::new(),
                        };
                        let r = if env.is_empty() {
                            crate::v5::ops::git::clone_repo(url, &dir)
                        } else {
                            crate::v5::ops::git::clone_repo_with_env(url, &dir, &env)
                        };
                        r.map_err(|e| e.to_string())?;
                        // Persist core.sshCommand on the new clone so
                        // subsequent fetch/pull/push reuses the key.
                        if let Some(p) = key_path.as_ref() {
                            let _ = crate::v5::ops::git::set_ssh_command(&dir, p);
                        }
                        Ok(())
                    }
                }).await;
                match result {
                    Ok(()) => { ok += 1; yield member_ok(&wire, "from_org"); }
                    Err(e) => { errored += 1; yield member_err(&wire, "from_org", e); }
                }
            }
            yield WorkspacesEvent::WorkspaceGitSummary {
                name: ws_name, op: "from_org".into(),
                total, ok, errored, not_members: Vec::new(), note: None,
            };
        }
    }

    // ==================================================================
    // V5PARITY-35: workspace-wide forges scoping.
    // ==================================================================

    #[plexus_macros::method(params(
        name = "Workspace name",
        forges = "Comma-separated providers (github,codeberg,gitlab) | 'none' (= []) | 'unset' (= null)",
        filter = "Glob filter on member names (comma-separated)",
        dry_run = "Preview without writing"
    ))]
    pub async fn set_forges(
        &self,
        name: String,
        forges: String,
        filter: Option<String>,
        dry_run: Option<serde_json::Value>,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            if name.is_empty() {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: "missing required parameter 'name'".into(),
                };
                return;
            }
            let dry = dry_run.as_ref().is_some_and(to_bool);
            let glob = match filter.as_deref().map(GlobSet::from_csv).transpose() {
                Ok(g) => g,
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("validation".into()),
                        message: format!("invalid filter: {e}"),
                    };
                    return;
                }
            };
            let (ws, loaded) = match load_iter_ctx(&config_dir, &name).await {
                Ok(t) => t, Err(e) => { yield e; return; }
            };
            let ws_path = std::path::PathBuf::from(ws.path.as_str());
            let mut total = 0u32; let mut ok = 0u32;
            let mut errored = 0u32; let mut unchanged = 0u32;
            for ctx in member_ctxs(&ws, &loaded, &ws_path) {
                if !glob.as_ref().map_or(true, |g| g.matches(&ctx.reference.name)) {
                    continue;
                }
                total += 1;
                // Delegate to repos::set_forges shape — call the same
                // helper directly to keep one implementation.
                let result = apply_set_forges(
                    &config_dir,
                    &ctx.reference.org,
                    &ctx.reference.name,
                    &forges,
                    dry,
                ).await;
                match result {
                    Ok((target_str, changed, local_path)) => {
                        if changed { ok += 1; } else { unchanged += 1; }
                        yield WorkspacesEvent::ForgesSet {
                            reference: ctx.reference.clone(),
                            forges: target_str,
                            changed,
                            dry_run: dry,
                            local_path,
                        };
                    }
                    Err(msg) => {
                        errored += 1;
                        yield member_err(&ctx.reference, "set_forges", msg);
                    }
                }
            }
            yield WorkspacesEvent::WorkspaceSetForgesSummary {
                name: ws.name.as_str().to_string(),
                total, ok, errored, unchanged,
                dry_run: dry,
            };
        }
    }

    // ==================================================================
    // Workspace-wide org migration.
    // ==================================================================

    /// Move workspace members to a different (already-registered) org.
    ///
    /// If `members` (JSON array of repo names) is given, migrate exactly
    /// those; otherwise migrate ALL members whose org == `from_org`. The
    /// target org `to_org` must already exist. Per-member failures are
    /// isolated — a failing member emits an `error` event and the batch
    /// continues. When a member is an `{ref, dir}` object, its local
    /// `dir` is passed through so the checkout's `.hyperforge/config.toml`
    /// org + git remotes get flipped too.
    #[plexus_macros::method(params(
        name = "Workspace name",
        from_org = "Source org to move members off of",
        to_org = "Target org (must already be registered)",
        members = "Optional JSON array of repo names to migrate (default: all members in from_org)",
        dry_run = "Preview only (default true); --dry-run false writes"
    ))]
    pub async fn migrate_org(
        &self,
        name: String,
        from_org: String,
        to_org: String,
        members: Option<String>,
        dry_run: Option<serde_json::Value>,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            if from_org.is_empty() || to_org.is_empty() {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: "missing required parameter 'from_org' or 'to_org'".into(),
                };
                return;
            }
            if from_org == to_org {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: "'to_org' must differ from 'from_org'".into(),
                };
                return;
            }
            let dry = dry_run.as_ref().map_or(true, to_bool);
            // Optional explicit member filter (repo names).
            let name_filter: Option<Vec<String>> = match members.as_deref().map(str::trim) {
                Some(s) if !s.is_empty() => {
                    match serde_json::from_str::<Vec<String>>(s) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            yield WorkspacesEvent::Error {
                                code: Some("validation".into()),
                                message: format!("members must be a JSON array of names: {e}"),
                            };
                            return;
                        }
                    }
                }
                _ => None,
            };
            let (ws, loaded) = match load_iter_ctx(&config_dir, &name).await {
                Ok(t) => t, Err(e) => { yield e; return; }
            };
            let ws_path = std::path::PathBuf::from(ws.path.as_str());
            let mut total = 0u32; let mut succeeded = 0u32; let mut errored = 0u32;
            for ctx in member_ctxs(&ws, &loaded, &ws_path) {
                // Select members: explicit name list, else all in from_org.
                let selected = match &name_filter {
                    Some(names) => names.iter().any(|n| n == &ctx.reference.name),
                    None => ctx.reference.org == from_org,
                };
                if !selected {
                    continue;
                }
                // An explicitly-named member not in from_org is a no-op
                // for migration but should still surface as an error so
                // the caller knows it wasn't moved.
                total += 1;
                let dir_opt = ctx.dir.exists().then(|| ctx.dir.as_path());
                match crate::v5::repos::migrate_one(
                    &config_dir,
                    &ctx.reference.org,
                    &ctx.reference.name,
                    &to_org,
                    // Generic workspace migrate-org assumes github as the
                    // migrating forge; doctor/heal pass the resolved
                    // provider. HYPE-6.
                    crate::v5::config::ProviderKind::Github,
                    dir_opt,
                    dry,
                ) {
                    Ok(outcome) => {
                        succeeded += 1;
                        yield WorkspacesEvent::MemberMigrated {
                            reference: crate::v5::repos::RepoRefWire {
                                org: to_org.clone(),
                                name: ctx.reference.name.clone(),
                            },
                            old_org: ctx.reference.org.clone(),
                            new_org: to_org.clone(),
                            local_path: outcome.local_path,
                            dry_run: dry,
                        };
                    }
                    Err(msg) => {
                        errored += 1;
                        let body = msg.split_once(": ").map_or(msg.as_str(), |(_, b)| b);
                        yield member_err(&ctx.reference, "migrate_org", body);
                    }
                }
            }
            yield WorkspacesEvent::WorkspaceMigrateOrgSummary {
                name: ws.name.as_str().to_string(),
                total, succeeded, errored,
                dry_run: dry,
            };
        }
    }

    // ==================================================================
    // Org-diff — discover what actually moved between two orgs.
    // ==================================================================

    /// Discover which workspace members ACTUALLY moved from `from_org` to
    /// `to_org` on the forge — the read-only discovery half that makes a
    /// partial org migration safe. For each member in scope, queries the
    /// forge for the repo's CANONICAL owner (following GitHub's
    /// rename/transfer redirect, equivalent to
    /// `gh api repos/<org>/<name> --jq .owner.login`) and buckets it:
    /// `moved` (canonical == to_org), `stayed` (== from_org),
    /// `elsewhere` (some other org), `missing` (not found on forge).
    ///
    /// Scope: members whose declared org == `from_org` (or all members if
    /// none match — but typically the workspace mixes both orgs after a
    /// partial move). Read-only — writes NO config, moves NO repos.
    /// Forge lookups are cached by repo name and per-repo errors are
    /// isolated (a 404/rename buckets the repo; it never aborts the diff).
    ///
    /// COMPOSITION: the emitted `OrgDiffSummary.moved` list is the EXACT
    /// `members` argument to pass to `workspaces.migrate_org(name,
    /// from_org, to_org, members=<moved>)` — diff first to discover, then
    /// migrate only what moved, never over-migrating the repos that stayed.
    #[plexus_macros::method(params(
        name = "Workspace name",
        from_org = "Source org the repos were declared under",
        to_org = "Target org to check whether each repo moved into"
    ))]
    pub async fn org_diff(
        &self,
        name: String,
        from_org: String,
        to_org: String,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            if from_org.is_empty() || to_org.is_empty() {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: "missing required parameter 'from_org' or 'to_org'".into(),
                };
                return;
            }
            if from_org == to_org {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: "'to_org' must differ from 'from_org'".into(),
                };
                return;
            }
            let (ws, loaded) = match load_iter_ctx(&config_dir, &name).await {
                Ok(t) => t, Err(e) => { yield e; return; }
            };
            let ws_path = std::path::PathBuf::from(ws.path.as_str());
            let resolver = crate::v5::secrets::YamlSecretStore::new(&config_dir);

            let mut total = 0u32;
            let mut moved: Vec<String> = Vec::new();
            let mut stayed: Vec<String> = Vec::new();
            let mut elsewhere: Vec<String> = Vec::new();
            let mut missing: Vec<String> = Vec::new();
            // Cache forge lookups by repo name (a workspace may list the
            // same repo twice across shapes; never re-hit the forge).
            let mut cache: std::collections::HashMap<String, WorkspacesEvent> =
                std::collections::HashMap::new();

            for ctx in member_ctxs(&ws, &loaded, &ws_path) {
                // Only diff members declared under from_org.
                if ctx.reference.org != from_org {
                    continue;
                }
                let repo_name = ctx.reference.name.clone();
                if let Some(cached) = cache.get(&repo_name) {
                    yield cached.clone();
                    continue;
                }
                total += 1;

                // Resolve the repo's canonical remote so we can pick a
                // provider + credentials. No remote → missing (we can't
                // ask the forge anything about it).
                let Some(repo) = ctx.repo else {
                    let entry = WorkspacesEvent::OrgDiffEntry {
                        name: repo_name.clone(),
                        declared_org: from_org.clone(),
                        canonical_org: None,
                        verdict: crate::v5::ops::repo::OrgDiffVerdict::Missing.as_str().into(),
                    };
                    missing.push(repo_name.clone());
                    cache.insert(repo_name.clone(), entry.clone());
                    yield entry;
                    continue;
                };
                let Some(remote) = crate::v5::ops::repo::canonical_remote_in_scope(
                    repo, &loaded.global.provider_map,
                ) else {
                    let entry = WorkspacesEvent::OrgDiffEntry {
                        name: repo_name.clone(),
                        declared_org: from_org.clone(),
                        canonical_org: None,
                        verdict: crate::v5::ops::repo::OrgDiffVerdict::Missing.as_str().into(),
                    };
                    missing.push(repo_name.clone());
                    cache.insert(repo_name.clone(), entry.clone());
                    yield entry;
                    continue;
                };
                let provider = match crate::v5::repos::derive_provider(remote, &loaded.global.provider_map) {
                    Ok(p) => p,
                    Err(e) => {
                        let err = WorkspacesEvent::Error {
                            code: Some("network".into()),
                            message: format!("{repo_name}: derive provider: {e}"),
                        };
                        cache.insert(repo_name.clone(), err.clone());
                        yield err;
                        continue;
                    }
                };
                let org_cfg = loaded.orgs.get(&crate::v5::config::OrgName::from(from_org.as_str()));
                let token_ref = org_cfg
                    .and_then(|o| crate::v5::ops::repo::token_ref_scoped(o, provider))
                    .map(|s| s.to_string());
                let fallback_token_ref =
                    Some(crate::v5::ops::repo::provider_default_token_ref(provider));
                let repo_ref = crate::v5::config::RepoRef {
                    org: crate::v5::config::OrgName::from(from_org.as_str()),
                    name: crate::v5::config::RepoName::from(repo_name.as_str()),
                };

                // Query the forge for the canonical owner (follows renames).
                let event = match crate::v5::ops::repo::canonical_owner_on_forge(
                    remote,
                    &repo_ref,
                    &loaded.global.provider_map,
                    &resolver,
                    token_ref.as_deref(),
                    fallback_token_ref,
                ).await {
                    Ok(owner) => {
                        // HYPE-5: compare canonical owners so an aliased owner
                        // (e.g. the forge answers `hypermemetic` for a repo
                        // registered under `hypermemetic-ai`) buckets as the
                        // same identity, not `Elsewhere`.
                        let verdict = crate::v5::ops::repo::OrgDiffVerdict::classify(
                            &loaded.global.canonical_owner(&owner),
                            &loaded.global.canonical_owner(&from_org),
                            &loaded.global.canonical_owner(&to_org),
                        );
                        match verdict {
                            crate::v5::ops::repo::OrgDiffVerdict::Moved => moved.push(repo_name.clone()),
                            crate::v5::ops::repo::OrgDiffVerdict::Stayed => stayed.push(repo_name.clone()),
                            crate::v5::ops::repo::OrgDiffVerdict::Elsewhere => elsewhere.push(repo_name.clone()),
                            crate::v5::ops::repo::OrgDiffVerdict::Missing => missing.push(repo_name.clone()),
                        }
                        WorkspacesEvent::OrgDiffEntry {
                            name: repo_name.clone(),
                            declared_org: from_org.clone(),
                            canonical_org: Some(owner),
                            verdict: verdict.as_str().into(),
                        }
                    }
                    Err(e) if e.class == crate::v5::adapters::ForgeErrorClass::NotFound => {
                        missing.push(repo_name.clone());
                        WorkspacesEvent::OrgDiffEntry {
                            name: repo_name.clone(),
                            declared_org: from_org.clone(),
                            canonical_org: None,
                            verdict: crate::v5::ops::repo::OrgDiffVerdict::Missing.as_str().into(),
                        }
                    }
                    Err(e) => {
                        // Auth/network/rate-limit: surface as an error
                        // event but keep diffing the rest of the batch.
                        WorkspacesEvent::Error {
                            code: Some(e.class.as_str().into()),
                            message: format!("{repo_name}: {}", e.message),
                        }
                    }
                };
                cache.insert(repo_name.clone(), event.clone());
                yield event;
            }

            yield WorkspacesEvent::OrgDiffSummary {
                from_org: from_org.clone(),
                to_org: to_org.clone(),
                total,
                moved,
                stayed,
                elsewhere,
                missing,
            };
        }
    }

    // ==================================================================
    // V5PARITY-4: workspace-level analytics aggregates.
    // ==================================================================

    #[plexus_macros::method(params(name = "Workspace name"))]
    pub async fn repo_sizes(
        &self,
        name: String,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        self.analytics_op(name, "size").await
    }

    #[plexus_macros::method(params(name = "Workspace name"))]
    pub async fn loc(
        &self,
        name: String,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        self.analytics_op(name, "loc").await
    }

    #[plexus_macros::method(params(
        name = "Workspace name",
        threshold = "Threshold in KB (default: 100)"
    ))]
    pub async fn large_files(
        &self,
        name: String,
        threshold: Option<u64>,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        self.analytics_op_large(name, threshold.unwrap_or(100) * 1024).await
    }

    #[plexus_macros::method(params(name = "Workspace name"))]
    pub async fn dirty(
        &self,
        name: String,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        self.analytics_op(name, "dirty").await
    }

    async fn analytics_op(
        &self,
        name: String,
        metric: &'static str,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            if name.is_empty() {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: "missing required parameter 'name'".into(),
                };
                return;
            }
            let ws = match crate::v5::ops::state::load_workspace(&config_dir, &name) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    yield WorkspacesEvent::Error {
                        code: Some("not_found".into()),
                        message: format!("workspace '{name}' not found"),
                    };
                    return;
                }
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let mut ok = 0u32;
            let mut errored = 0u32;
            let total = u32::try_from(ws.repos.len()).unwrap_or(u32::MAX);
            let ws_path = std::path::PathBuf::from(ws.path.as_str());
            let mut total_bytes: u64 = 0;
            let mut total_file_count: u64 = 0;
            let mut total_loc: u64 = 0;
            let mut dirty_count: u32 = 0;
            for entry in &ws.repos {
                let (org_s, name_s) = workspace_member_ref(entry);
                let wire = crate::v5::repos::RepoRefWire {
                    org: org_s.clone(),
                    name: name_s.clone(),
                };
                let dir_name = match entry {
                    crate::v5::config::WorkspaceRepo::Object { dir, .. } => dir.clone(),
                    _ => name_s.clone(),
                };
                let dir = ws_path.join(&dir_name);
                match metric {
                    "size" => match crate::v5::ops::analytics::repo_size(&dir) {
                        Ok(s) => {
                            ok += 1;
                            total_bytes += s.bytes;
                            total_file_count += s.file_count;
                            yield WorkspacesEvent::MemberAnalytics {
                                reference: wire, metric: metric.into(),
                                bytes: Some(s.bytes), file_count: Some(s.file_count),
                                loc_total: None, large_count: None, dirty: None,
                                status: Some("ok".into()), message: None,
                            };
                        }
                        Err(e) => {
                            errored += 1;
                            yield WorkspacesEvent::MemberAnalytics {
                                reference: wire, metric: metric.into(),
                                bytes: None, file_count: None,
                                loc_total: None, large_count: None, dirty: None,
                                status: Some("errored".into()), message: Some(e.to_string()),
                            };
                        }
                    },
                    "loc" => match crate::v5::ops::analytics::repo_loc(&dir) {
                        Ok(m) => {
                            ok += 1;
                            let t: u64 = m.values().sum();
                            total_loc += t;
                            yield WorkspacesEvent::MemberAnalytics {
                                reference: wire, metric: metric.into(),
                                bytes: None, file_count: None,
                                loc_total: Some(t), large_count: None, dirty: None,
                                status: Some("ok".into()), message: None,
                            };
                        }
                        Err(e) => {
                            errored += 1;
                            yield WorkspacesEvent::MemberAnalytics {
                                reference: wire, metric: metric.into(),
                                bytes: None, file_count: None,
                                loc_total: None, large_count: None, dirty: None,
                                status: Some("errored".into()), message: Some(e.to_string()),
                            };
                        }
                    },
                    "dirty" => match crate::v5::ops::git::is_dirty(&dir) {
                        Ok(d) => {
                            ok += 1;
                            if d { dirty_count += 1; }
                            yield WorkspacesEvent::MemberAnalytics {
                                reference: wire, metric: metric.into(),
                                bytes: None, file_count: None,
                                loc_total: None, large_count: None, dirty: Some(d),
                                status: Some("ok".into()), message: None,
                            };
                        }
                        Err(e) => {
                            errored += 1;
                            yield WorkspacesEvent::MemberAnalytics {
                                reference: wire, metric: metric.into(),
                                bytes: None, file_count: None,
                                loc_total: None, large_count: None, dirty: None,
                                status: Some("errored".into()), message: Some(e.to_string()),
                            };
                        }
                    },
                    _ => unreachable!("analytics_op called with unknown metric: {metric}"),
                }
            }
            yield WorkspacesEvent::WorkspaceAnalyticsSummary {
                name: ws.name.as_str().to_string(),
                metric: metric.into(),
                total, ok, errored,
                total_bytes: if metric == "size" { Some(total_bytes) } else { None },
                total_file_count: if metric == "size" { Some(total_file_count) } else { None },
                total_loc: if metric == "loc" { Some(total_loc) } else { None },
                total_large_files: None,
                dirty_count: if metric == "dirty" { Some(dirty_count) } else { None },
            };
        }
    }

    async fn analytics_op_large(
        &self,
        name: String,
        threshold_bytes: u64,
    ) -> impl futures::Stream<Item = WorkspacesEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        async_stream::stream! {
            if name.is_empty() {
                yield WorkspacesEvent::Error {
                    code: Some("validation".into()),
                    message: "missing required parameter 'name'".into(),
                };
                return;
            }
            let ws = match crate::v5::ops::state::load_workspace(&config_dir, &name) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    yield WorkspacesEvent::Error {
                        code: Some("not_found".into()),
                        message: format!("workspace '{name}' not found"),
                    };
                    return;
                }
                Err(e) => {
                    yield WorkspacesEvent::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let mut ok = 0u32;
            let mut errored = 0u32;
            let total = u32::try_from(ws.repos.len()).unwrap_or(u32::MAX);
            let ws_path = std::path::PathBuf::from(ws.path.as_str());
            let mut total_large: u64 = 0;
            for entry in &ws.repos {
                let (org_s, name_s) = workspace_member_ref(entry);
                let wire = crate::v5::repos::RepoRefWire {
                    org: org_s.clone(), name: name_s.clone(),
                };
                let dir_name = match entry {
                    crate::v5::config::WorkspaceRepo::Object { dir, .. } => dir.clone(),
                    _ => name_s.clone(),
                };
                let dir = ws_path.join(&dir_name);
                match crate::v5::ops::analytics::large_files(&dir, threshold_bytes) {
                    Ok(items) => {
                        ok += 1;
                        let cnt = items.len() as u64;
                        total_large += cnt;
                        yield WorkspacesEvent::MemberAnalytics {
                            reference: wire, metric: "large_files".into(),
                            bytes: None, file_count: None,
                            loc_total: None, large_count: Some(cnt), dirty: None,
                            status: Some("ok".into()), message: None,
                        };
                    }
                    Err(e) => {
                        errored += 1;
                        yield WorkspacesEvent::MemberAnalytics {
                            reference: wire, metric: "large_files".into(),
                            bytes: None, file_count: None,
                            loc_total: None, large_count: None, dirty: None,
                            status: Some("errored".into()), message: Some(e.to_string()),
                        };
                    }
                }
            }
            yield WorkspacesEvent::WorkspaceAnalyticsSummary {
                name: ws.name.as_str().to_string(),
                metric: "large_files".into(),
                total, ok, errored,
                total_bytes: None, total_file_count: None,
                total_loc: None, total_large_files: Some(total_large),
                dirty_count: None,
            };
        }
    }
}

/// Extract `(org, name)` from a WorkspaceRepo. Shorthand `"org/name"`
/// and the object form both route through this.
fn workspace_member_ref(entry: &crate::v5::config::WorkspaceRepo) -> (String, String) {
    match entry {
        crate::v5::config::WorkspaceRepo::Shorthand(s) => {
            s.split_once('/')
                .map(|(o, n)| (o.to_string(), n.to_string()))
                .unwrap_or_else(|| (String::new(), s.clone()))
        }
        crate::v5::config::WorkspaceRepo::Object { reference, .. } => (
            reference.org.as_str().to_string(),
            reference.name.as_str().to_string(),
        ),
    }
}

// ---------------------------------------------------------------------
// V5PARITY-14: typed helpers shared by the workspace git verbs.
// Each verb method has its own loop; these helpers carry the shared
// pieces (load + decode) without becoming a string-dispatch service.
// ---------------------------------------------------------------------

/// Per-member iteration context. `repo` is `None` when the member's
/// org/repo can't be resolved against the loaded config — verbs that
/// need the canonical remote (like `clone`) check this; verbs that
/// only need the on-disk `dir` (like `fetch`/`status`) ignore it.
struct MemberCtx<'a> {
    reference: crate::v5::repos::RepoRefWire,
    repo: Option<&'a crate::v5::config::OrgRepo>,
    dir: std::path::PathBuf,
}

/// Load workspace + the global `LoadedConfig` for an iteration. On any
/// failure returns the wire `Error` event the caller should yield.
async fn load_iter_ctx(
    config_dir: &std::path::Path,
    name: &str,
) -> Result<(crate::v5::config::WorkspaceConfig, crate::v5::config::LoadedConfig), WorkspacesEvent> {
    if name.is_empty() {
        return Err(WorkspacesEvent::Error {
            code: Some("validation".into()),
            message: "missing required parameter 'name'".into(),
        });
    }
    let ws = match crate::v5::ops::state::load_workspace(config_dir, name) {
        Ok(Some(c)) => c,
        Ok(None) => return Err(WorkspacesEvent::Error {
            code: Some("not_found".into()),
            message: format!("workspace '{name}' not found"),
        }),
        Err(e) => return Err(WorkspacesEvent::Error {
            code: Some("config_error".into()),
            message: e.to_string(),
        }),
    };
    let loaded = crate::v5::ops::state::load_all(config_dir)
        .map_err(|e| WorkspacesEvent::Error {
            code: Some("config_error".into()),
            message: e.to_string(),
        })?;
    Ok((ws, loaded))
}

/// Iterate decoded member contexts. Skips shorthand entries that don't
/// have `<org>/<name>` shape.
fn member_ctxs<'a>(
    ws: &'a crate::v5::config::WorkspaceConfig,
    loaded: &'a crate::v5::config::LoadedConfig,
    ws_path: &'a std::path::Path,
) -> impl Iterator<Item = MemberCtx<'a>> + 'a {
    ws.repos.iter().filter_map(|entry| {
        let (org_s, name_s) = match entry {
            crate::v5::config::WorkspaceRepo::Shorthand(s) => s.split_once('/')
                .map(|(o, n)| (o.to_string(), n.to_string()))?,
            crate::v5::config::WorkspaceRepo::Object { reference, .. } => (
                reference.org.as_str().to_string(),
                reference.name.as_str().to_string(),
            ),
        };
        let dir_name = match entry {
            crate::v5::config::WorkspaceRepo::Object { dir, .. } => dir.clone(),
            _ => name_s.clone(),
        };
        let dir = ws_path.join(&dir_name);
        let repo = loaded.orgs
            .get(&crate::v5::config::OrgName::from(org_s.as_str()))
            .and_then(|o| crate::v5::ops::state::find_repo(o, &name_s));
        Some(MemberCtx {
            reference: crate::v5::repos::RepoRefWire { org: org_s, name: name_s },
            repo,
            dir,
        })
    })
}

/// HYPERFORGE-64 (3): the git clones directly under `ws_path` that are not
/// members, by directory name, sorted. A clone has a `.git` directory; a
/// linked worktree (a `.git` file) belongs to its repo and is not counted.
fn checkouts_not_members(ws_path: &std::path::Path, member_dirs: &[std::path::PathBuf]) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(ws_path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join(".git").is_dir() && !member_dirs.iter().any(|m| m == p))
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .collect();
    out.sort();
    out
}

/// The summary's sentence for [`checkouts_not_members`], if any.
fn stray_note(ws_path: &std::path::Path, strays: &[String]) -> Option<String> {
    match strays.len() {
        0 => None,
        1 => Some(format!(
            "1 git checkout under {} is not a member and was not pulled: {} (workspaces add_repo to include it)",
            ws_path.display(), strays[0],
        )),
        n => Some(format!(
            "{n} git checkouts under {} are not members and were not pulled: {} (workspaces add_repo to include them)",
            ws_path.display(), strays.join(", "),
        )),
    }
}

/// A ticket id is safe as a branch and a folder name.
fn valid_ticket(t: &str) -> bool {
    !t.is_empty()
        && !t.starts_with(['-', '.'])
        && !t.contains("..")
        && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// One checkout of a member repo: its main checkout or a worktree.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OrientRow {
    pub repo: String,
    pub path: String,
    pub worktree: bool,
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Files staged, changed or untracked.
    pub dirty: u32,
    pub error: Option<String>,
}

impl OrientRow {
    fn needs_attention(&self) -> bool {
        self.dirty > 0 || self.ahead > 0 || self.behind > 0 || self.upstream.is_none() || self.error.is_some()
    }
}

fn orient_table(rows: &[OrientRow]) -> String {
    let cells: Vec<[String; 5]> = rows.iter().map(|r| [
        if r.worktree { format!("{} (wt)", r.repo) } else { r.repo.clone() },
        r.branch.clone().unwrap_or_else(|| "(detached)".into()),
        r.upstream.as_ref().map_or_else(|| "no upstream".into(), |_| format!("+{} -{}", r.ahead, r.behind)),
        if r.dirty > 0 { format!("{} dirty", r.dirty) } else { "clean".into() },
        r.error.clone().unwrap_or_else(|| r.path.clone()),
    ]).collect();
    let header = ["REPO", "BRANCH", "UPSTREAM", "TREE", "PATH"].map(String::from);
    let width = |c: usize| cells.iter().chain([&header]).map(|r| r[c].chars().count()).max().unwrap_or(0);
    let widths = [width(0), width(1), width(2), width(3)];
    let line = |r: &[String; 5]| format!(
        "{:w0$}  {:w1$}  {:w2$}  {:w3$}  {}\n", r[0], r[1], r[2], r[3], r[4],
        w0 = widths[0], w1 = widths[1], w2 = widths[2], w3 = widths[3]);
    let mut out = line(&header);
    for r in &cells {
        out.push_str(&line(r));
    }
    out
}

fn member_ok(reference: &crate::v5::repos::RepoRefWire, op: &str) -> WorkspacesEvent {
    WorkspacesEvent::MemberGitResult {
        reference: reference.clone(),
        op: op.into(),
        status: "ok".into(),
        message: None,
    }
}

fn member_err(reference: &crate::v5::repos::RepoRefWire, op: &str, msg: impl Into<String>) -> WorkspacesEvent {
    WorkspacesEvent::MemberGitResult {
        reference: reference.clone(),
        op: op.into(),
        status: "errored".into(),
        message: Some(msg.into()),
    }
}

/// Coerce a JSON value to bool. Accepts `true`/false`, the strings
/// `"true"`/`"1"`/`"yes"` (lowercase), or returns the supplied default.
fn to_bool(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::String(s) => matches!(s.as_str(), "true" | "1" | "yes"),
        _ => false,
    }
}

/// V5PARITY-22: minimal glob matcher. Supports `*` (any chars), `?`
/// (single char). Comma-separated patterns via `from_csv` are OR'd.
/// Lifted to its own helper rather than pulling in the `glob` crate
/// since v5's filter needs are basic.
struct GlobSet {
    patterns: Vec<String>,
}

impl GlobSet {
    fn from_csv(s: &str) -> Result<Self, String> {
        let patterns: Vec<String> = s.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        if patterns.is_empty() {
            return Err("empty filter".into());
        }
        Ok(Self { patterns })
    }
    fn matches(&self, name: &str) -> bool {
        self.patterns.iter().any(|p| glob_match(p, name))
    }
}

fn glob_match(pattern: &str, s: &str) -> bool {
    // Iterative DP over (pattern_idx, str_idx).
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let mut pi = 0usize;
    let mut ti = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_ti = 0usize;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1; ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// V5PARITY-35: shared logic for `workspaces.set_forges`. Returns
/// `(forges_as_strings, changed, local_path_if_written)` or an error
/// message on failure. Mirrors the body of `repos.set_forges` so both
/// paths land the same result on disk.
async fn apply_set_forges(
    config_dir: &std::path::Path,
    org: &str,
    name: &str,
    forges: &str,
    dry: bool,
) -> Result<(Option<Vec<String>>, bool, Option<String>), String> {
    let target = crate::v5::repos::parse_forges_arg(forges)
        .map_err(|e| e.to_string())?;
    let loaded = crate::v5::ops::state::load_all(config_dir)
        .map_err(|e| e.to_string())?;
    let org_key = crate::v5::config::OrgName::from(org);
    let existing = loaded.orgs.get(&org_key).cloned()
        .ok_or_else(|| format!("org '{org}' not found"))?;
    if crate::v5::ops::state::find_repo(&existing, name).is_none() {
        return Err(format!("repo '{name}' not in org '{org}'"));
    }
    let mut updated = existing.clone();
    let mut changed = false;
    if let Some(mr) = crate::v5::ops::state::find_repo_mut(&mut updated, name) {
        if mr.forges != target {
            mr.forges = target.clone();
            changed = true;
        }
    }
    if changed && !dry {
        let orgs_dir = config_dir.join("orgs");
        crate::v5::ops::state::save_org(&orgs_dir, &updated)
            .map_err(|e| e.to_string())?;
    }
    let local_path = if dry { None } else {
        crate::v5::repos::write_through_repo_config(config_dir, org, name, target.as_deref(), &loaded)
    };
    let target_str = target.map(|v| v.into_iter().map(|p| match p {
        crate::v5::config::ProviderKind::Github => "github".to_string(),
        crate::v5::config::ProviderKind::Codeberg => "codeberg".to_string(),
        crate::v5::config::ProviderKind::Gitlab => "gitlab".to_string(),
    }).collect());
    Ok((target_str, changed, local_path))
}

/// V5PARITY-22 helper: resolve the org's SSH private key path. Mirrors
/// `repos.rs::ssh_key_for_org` (different module visibility, same shape).
fn ssh_key_path_for_org(org: &crate::v5::config::OrgConfig) -> Option<std::path::PathBuf> {
    org.forge.credentials.iter()
        .find(|c| matches!(c.cred_type, crate::v5::config::CredentialType::SshKey))
        .map(|c| {
            let raw = c.key.as_str();
            if let Some(rest) = raw.strip_prefix("~/") {
                if let Some(home) = std::env::var_os("HOME") {
                    return std::path::PathBuf::from(home).join(rest);
                }
            }
            std::path::PathBuf::from(raw)
        })
}

/// Read `origin` URL via `ops::git::read_origin_url` (V5PARITY-15).
/// Backed by git2 for in-process speed; falls back to `git config
/// --get` under `HF_GIT_FORCE_SUBPROCESS=1`.
fn read_git_origin(dir: &std::path::Path) -> Option<String> {
    crate::v5::ops::git::read_origin_url(dir).ok().flatten()
}

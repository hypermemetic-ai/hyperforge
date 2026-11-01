//! `BuildService` — V5PARITY-9/10/11. Wire surface for manifest inspection,
//! release flow, distribution scaffolding, and per-repo command exec.

use std::path::PathBuf;

use async_stream::stream;
use futures::Stream;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v5::build::{diff, dist, exec, manifest, release};
use crate::v5::config::{OrgName, WorkspaceRepo};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BuildEvent {
    // V5PARITY-9 manifest events.
    PackageManifest {
        #[serde(rename = "ref")]
        reference: crate::v5::repos::RepoRefWire,
        kind: String,
        name: String,
        version: String,
        deps: Vec<manifest::Dep>,
    },
    AnalyzeFinding {
        #[serde(flatten)]
        finding: manifest::Finding,
    },
    ValidateOk { name: String, total: u32 },
    ValidateFailed { name: String, total: u32, failed: u32 },
    NameMismatch {
        #[serde(rename = "ref")]
        reference: crate::v5::repos::RepoRefWire,
        manifest_name: String,
    },
    PackageDiffEntry {
        #[serde(flatten)]
        change: diff::PackageChange,
    },

    // V5PARITY-10 release events.
    VersionBumped {
        #[serde(rename = "ref")]
        reference: crate::v5::repos::RepoRefWire,
        old: String,
        new: String,
    },
    PackagePublished {
        #[serde(rename = "ref")]
        reference: crate::v5::repos::RepoRefWire,
        channel: String,
    },
    ReleaseCreated {
        #[serde(rename = "ref")]
        reference: crate::v5::repos::RepoRefWire,
        tag: String,
    },
    /// HYPERFORGE-44: what a release would do; nothing was written.
    ReleasePlanned {
        #[serde(rename = "ref")]
        reference: crate::v5::repos::RepoRefWire,
        old: String,
        new: String,
        tag: String,
        branch: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
    },
    ReleaseSummary {
        name: String,
        total: u32,
        ok: u32,
        errored: u32,
        /// false: a dry run, `ok` counts plans.
        executed: bool,
    },

    // V5PARITY-11 dist/exec events.
    DistInit {
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        reference: Option<crate::v5::repos::RepoRefWire>,
        path: String,
        created: bool,
    },
    DistShow {
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        reference: Option<crate::v5::repos::RepoRefWire>,
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    BinstallInit {
        path: String,
        modified: bool,
    },
    BrewFormula {
        name: String,
        version: String,
        #[serde(skip_serializing_if = "std::ops::Not::not", default)]
        dry_run: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        written_to: Option<String>,
        content: String,
    },
    ExecOutput {
        #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
        reference: Option<crate::v5::repos::RepoRefWire>,
        path: String,
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    ExecSummary {
        name: String,
        total: u32,
        ok: u32,
        errored: u32,
    },

    // HYPERFORGE-23 crate-chain events.
    /// Something the chain run noticed (a crate added by the closure, a
    /// lockfile not refreshed, …).
    ChainNote { message: String },
    /// `package_diff`: one crate against the registry. `status`:
    /// `unpublished` · `ahead` (a version crates.io lacks) · `published`
    /// (and its source unchanged since) · `drifted` (published, but the
    /// source changed since — bump it) · `behind` · `yanked`.
    CrateState {
        name: String,
        version: String,
        repo: String,
        path: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        latest: Option<String>,
        changed_count: u32,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        changed: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        published_from: Option<String>,
        publish: bool,
    },
    CrateBumped {
        name: String,
        repo: String,
        old: String,
        new: String,
        dry_run: bool,
    },
    RequirementBumped {
        manifest: String,
        repo: String,
        key: String,
        package: String,
        table: String,
        old: String,
        new: String,
        dry_run: bool,
    },
    BumpCommitted {
        repo: String,
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
        files: Vec<String>,
    },
    /// `publish`: the plan, one per crate in dependency order.
    PublishPlanned {
        position: u32,
        group: u32,
        name: String,
        version: String,
        repo: String,
        /// `publish` | `skip`
        action: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// `publish`: a group's dry run or publish — `started`, then `ok` / `failed`.
    PublishGroup {
        group: u32,
        repo: String,
        crates: Vec<String>,
        /// `dry_run` | `execute`
        phase: String,
        state: String,
        step: String,
        log: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tail: Option<String>,
    },
    CrateTagged { name: String, repo: String, tag: String },
    PublishSummary {
        execute: bool,
        planned: u32,
        skipped: u32,
        verified: u32,
        published: Vec<String>,
        failed: Vec<String>,
        not_published: Vec<String>,
    },

    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        message: String,
    },
}

#[derive(Clone)]
pub struct BuildService {
    config_dir: PathBuf,
    state_dir: PathBuf,
}

impl BuildService {
    /// Logs of `publish` go under `<config_dir>/state/publish`.
    #[must_use]
    pub fn new(config_dir: PathBuf) -> Self {
        let state_dir = config_dir.join("state");
        Self { config_dir, state_dir }
    }

    /// HYPERFORGE-23: logs of `publish` go under `<state_dir>/publish`.
    #[must_use]
    pub const fn with_state_dir(config_dir: PathBuf, state_dir: PathBuf) -> Self {
        Self { config_dir, state_dir }
    }
}

/// Stream the events a chain task sends.
fn from_task<F, Fut>(f: F) -> impl Stream<Item = BuildEvent> + Send + 'static
where
    F: FnOnce(tokio::sync::mpsc::UnboundedSender<BuildEvent>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(f(tx));
    stream! {
        while let Some(ev) = rx.recv().await {
            yield ev;
        }
    }
}

/// A workspace's members as `(org/name, checkout)`.
fn chain_repos(config_dir: &std::path::Path, name: &str) -> Result<Vec<(String, PathBuf)>, String> {
    Ok(workspace_members(config_dir, name)?
        .into_iter()
        .map(|(r, d)| (format!("{}/{}", r.org, r.name), d))
        .collect())
}

fn resolve_store_secret(config_dir: &std::path::Path, reference: &str) -> Option<String> {
    use crate::v5::secrets::SecretResolver as _;
    let resolver = crate::v5::secrets::YamlSecretStore::new(config_dir);
    let parsed = crate::v5::secrets::SecretRef::parse(reference).ok()?;
    resolver.resolve(&parsed).ok()
}

/// HYPERFORGE-26: is this one-repo publish a Hackage one? With `--channel
/// hackage`, or no channel on a repo with a `.cabal` file and no Cargo.toml.
/// Returns the package's name.
fn one_repo_cabal_package(dir: &std::path::Path, channel: Option<&str>) -> Result<Option<String>, String> {
    let asked = channel == Some("hackage");
    if !asked && (channel.is_some() || dir.join("Cargo.toml").is_file()) {
        return Ok(None);
    }
    match crate::build_system::cabal::cabal_package_name(dir) {
        Some(name) => Ok(Some(name)),
        None if asked => Err(format!("no .cabal package in {}", dir.display())),
        None => Ok(None),
    }
}

fn err<S: Into<String>>(code: S, msg: impl Into<String>) -> BuildEvent {
    BuildEvent::Error {
        code: Some(code.into()),
        message: msg.into(),
    }
}

/// Resolve a workspace `name` → list of `(ref, checkout_dir)`.
fn workspace_members(
    config_dir: &std::path::Path,
    name: &str,
) -> Result<Vec<(crate::v5::repos::RepoRefWire, PathBuf)>, String> {
    let ws = crate::v5::ops::state::load_workspace(config_dir, name)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("workspace '{name}' not found"))?;
    let ws_path = PathBuf::from(ws.path.as_str());
    let mut out: Vec<(crate::v5::repos::RepoRefWire, PathBuf)> = Vec::new();
    for entry in &ws.repos {
        let (org, rname, dir) = match entry {
            WorkspaceRepo::Shorthand(s) => {
                match s.split_once('/') {
                    Some((o, n)) => (o.to_string(), n.to_string(), n.to_string()),
                    None => continue,
                }
            }
            WorkspaceRepo::Object { reference, dir, .. } => (
                reference.org.as_str().to_string(),
                reference.name.as_str().to_string(),
                dir.clone(),
            ),
        };
        out.push((
            crate::v5::repos::RepoRefWire { org, name: rname },
            ws_path.join(dir),
        ));
    }
    Ok(out)
}

/// Resolve `org/name` → checkout dir by looking at workspace membership.
/// Returns the first match (repos.yaml doesn't track checkout paths directly).
fn resolve_single_repo_dir(
    config_dir: &std::path::Path,
    org: &str,
    name: &str,
) -> Result<PathBuf, String> {
    let ws_dir = config_dir.join("workspaces");
    let wss = if ws_dir.is_dir() {
        crate::v5::ops::state::load_workspaces(&ws_dir).map_err(|e| e.to_string())?
    } else {
        std::collections::BTreeMap::new()
    };
    for (_, ws) in &wss {
        let ws_path = PathBuf::from(ws.path.as_str());
        for entry in &ws.repos {
            match entry {
                WorkspaceRepo::Shorthand(s) => {
                    if let Some((o, n)) = s.split_once('/') {
                        if o == org && n == name {
                            return Ok(ws_path.join(n));
                        }
                    }
                }
                WorkspaceRepo::Object { reference, dir, .. } => {
                    if reference.org.as_str() == org && reference.name.as_str() == name {
                        return Ok(ws_path.join(dir));
                    }
                }
            }
        }
    }
    Err(format!("no workspace lists {org}/{name}"))
}

#[plexus_macros::activation(
    trait = "BuildApi",
    methods = marked,
    namespace = "build",
    description = "v5 build/release/dist wire surface (V5PARITY-9/10/11)",
    crate_path = "plexus_core"
)]
impl BuildService {
    // ==================================================================
    // V5PARITY-9: manifest.
    // ==================================================================

    #[plexus_macros::method(params(name = "Workspace name"))]
    pub async fn unify(
        &self,
        name: String,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let members = match workspace_members(&config_dir, &name) {
                Ok(v) => v, Err(e) => { yield err("not_found", e); return; }
            };
            for (r, dir) in members {
                match manifest::detect_and_parse(&dir) {
                    Ok(m) => yield BuildEvent::PackageManifest {
                        reference: r, kind: m.kind, name: m.name, version: m.version, deps: m.deps,
                    },
                    Err(e) => yield err(e.code(), e.to_string()),
                }
            }
        }
    }

    #[plexus_macros::method(params(name = "Workspace name"))]
    pub async fn analyze(
        &self,
        name: String,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let members = match workspace_members(&config_dir, &name) {
                Ok(v) => v, Err(e) => { yield err("not_found", e); return; }
            };
            let mut manifests: Vec<manifest::PackageManifest> = Vec::new();
            for (_, dir) in &members {
                if let Ok(m) = manifest::detect_and_parse(dir) {
                    manifests.push(m);
                }
            }
            for f in manifest::analyze(&manifests) {
                yield BuildEvent::AnalyzeFinding { finding: f };
            }
        }
    }

    #[plexus_macros::method(params(name = "Workspace name"))]
    pub async fn validate(
        &self,
        name: String,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let members = match workspace_members(&config_dir, &name) {
                Ok(v) => v, Err(e) => { yield err("not_found", e); return; }
            };
            let total = u32::try_from(members.len()).unwrap_or(u32::MAX);
            let mut failed = 0u32;
            for (r, dir) in &members {
                if let Err(e) = manifest::detect_and_parse(dir) {
                    failed += 1;
                    yield BuildEvent::Error {
                        code: Some(e.code().into()),
                        message: format!("{}/{}: {}", r.org, r.name, e),
                    };
                }
            }
            if failed == 0 {
                yield BuildEvent::ValidateOk { name, total };
            } else {
                yield BuildEvent::ValidateFailed { name, total, failed };
            }
        }
    }

    #[plexus_macros::method(params(name = "Workspace name"))]
    pub async fn detect_name_mismatches(
        &self,
        name: String,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let members = match workspace_members(&config_dir, &name) {
                Ok(v) => v, Err(e) => { yield err("not_found", e); return; }
            };
            for (r, dir) in members {
                match manifest::detect_and_parse(&dir) {
                    Ok(m) if !m.name.is_empty() && m.name != r.name => {
                        yield BuildEvent::NameMismatch {
                            reference: r,
                            manifest_name: m.name,
                        };
                    }
                    _ => {}
                }
            }
        }
    }

    #[plexus_macros::method(params(
        name = "Workspace name",
        from_ref = "Git ref to diff from (with to_ref: the manifests' diff between two refs)",
        to_ref = "Git ref to diff to",
        include = "Without refs: crates to compare with crates.io (globs on crate names; default all)",
        exclude = "Crates to leave out (globs)"
    ))]
    pub async fn package_diff(
        &self,
        name: String,
        from_ref: Option<String>,
        to_ref: Option<String>,
        include: Option<Vec<String>>,
        exclude: Option<Vec<String>>,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        use futures::StreamExt as _;
        let config_dir = self.config_dir.clone();
        let (from_ref, to_ref) = match (from_ref, to_ref) {
            (Some(f), Some(t)) => (f, t),
            (None, None) => {
                // HYPERFORGE-23: each crate's version against crates.io.
                let repos = match chain_repos(&config_dir, &name) {
                    Ok(r) => r,
                    Err(e) => return futures::stream::once(async move { err("not_found", e) }).boxed(),
                };
                return from_task(move |tx| crate::v5::build::chain_ops::package_diff(
                    tx, repos, include.unwrap_or_default(), exclude.unwrap_or_default(),
                )).boxed();
            }
            _ => return futures::stream::once(async { err("validation", "from_ref and to_ref go together") }).boxed(),
        };
        stream! {
            let members = match workspace_members(&config_dir, &name) {
                Ok(v) => v, Err(e) => { yield err("not_found", e); return; }
            };
            for (r, dir) in members {
                // Pull the Cargo.toml at both refs via `git show`.
                let from_raw = match crate::v5::ops::git::show(&dir, &from_ref, "Cargo.toml") {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let to_raw = match crate::v5::ops::git::show(&dir, &to_ref, "Cargo.toml") {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let from = match manifest::parse_cargo_str(&from_raw, &dir.join("Cargo.toml")) {
                    Ok(m) => vec![m],
                    Err(_) => Vec::new(),
                };
                let to = match manifest::parse_cargo_str(&to_raw, &dir.join("Cargo.toml")) {
                    Ok(m) => vec![m],
                    Err(_) => Vec::new(),
                };
                for ch in diff::diff(&from, &to) {
                    yield BuildEvent::PackageDiffEntry { change: ch };
                }
                let _ = r;
            }
        }.boxed()
    }

    // ==================================================================
    // V5PARITY-10: release.
    // ==================================================================

    #[plexus_macros::method(params(
        org = "Org name (one repo's root Cargo.toml; leave out with include)",
        name = "Repo name, or with include the workspace name",
        bump = "major | minor | patch (ignored if --to is set)",
        to = "Exact target version",
        include = "Crates to bump across the workspace (globs on crate names): members of cargo workspaces too, every dependent's version requirement carried, one commit per repo",
        exclude = "Crates to leave out (globs)",
        closure = "Also bump every chain crate the included ones need (default false)",
        dry_run = "Say what would change; write nothing (default false)",
        commit = "Commit each repo's change (default true)"
    ))]
    #[allow(clippy::too_many_arguments)]
    pub async fn bump(
        &self,
        org: Option<String>,
        name: String,
        bump: Option<String>,
        to: Option<String>,
        include: Option<Vec<String>>,
        exclude: Option<Vec<String>>,
        closure: Option<bool>,
        dry_run: Option<bool>,
        commit: Option<bool>,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        use futures::StreamExt as _;
        let config_dir = self.config_dir.clone();
        let kind_or_target = to.clone().or_else(|| bump.clone()).unwrap_or_else(|| "patch".into());
        if let Some(include) = include {
            // HYPERFORGE-23: a set of crates across the workspace `name`.
            let repos = match chain_repos(&config_dir, &name) {
                Ok(r) => r,
                Err(e) => return futures::stream::once(async move { err("not_found", e) }).boxed(),
            };
            let args = crate::v5::build::chain_ops::BumpArgs {
                repos,
                include,
                exclude: exclude.unwrap_or_default(),
                closure: closure.unwrap_or(false),
                kind_or_target,
                dry_run: dry_run.unwrap_or(false),
                commit: commit.unwrap_or(true),
            };
            return from_task(move |tx| crate::v5::build::chain_ops::bump(tx, args)).boxed();
        }
        let Some(org) = org else {
            return futures::stream::once(async { err("validation", "org is required (or include, for a set of crates)") }).boxed();
        };
        stream! {
            let dir = match resolve_single_repo_dir(&config_dir, &org, &name) {
                Ok(d) => d, Err(e) => { yield err("not_found", e); return; }
            };
            let cargo = dir.join("Cargo.toml");
            if !cargo.is_file() {
                yield err("manifest_not_found", cargo.display().to_string());
                return;
            }
            let raw = match std::fs::read_to_string(&cargo) {
                Ok(s) => s, Err(e) => { yield err("io", e.to_string()); return; }
            };
            let kind_or_target = match (to.as_deref(), bump.as_deref()) {
                (Some(t), _) => t.to_string(),
                (None, Some(k)) => k.to_string(),
                (None, None) => "patch".into(),
            };
            let (old, new, new_text) = match release::bump_cargo_toml(&raw, &kind_or_target) {
                Ok(t) => t,
                Err(e) => { yield err(e.code(), e.to_string()); return; }
            };
            if let Err(e) = std::fs::write(&cargo, new_text) {
                yield err("io", e.to_string()); return;
            }
            // Commit + tag via ops::git (D13).
            let _ = crate::v5::ops::git::add(&dir, &["Cargo.toml"]);
            let _ = crate::v5::ops::git::commit(&dir, &format!("chore: bump version to {new}"));
            let _ = crate::v5::ops::git::tag(&dir, &format!("v{new}"));
            yield BuildEvent::VersionBumped {
                reference: crate::v5::repos::RepoRefWire { org, name },
                old, new,
            };
        }.boxed()
    }

    #[plexus_macros::method(params(
        org = "Org name (one repo, as before; leave out to publish a crate chain)",
        name = "Repo name, or without org the workspace name",
        channel = "crates.io | npm | pypi | hackage (one repo only; a repo with a .cabal file and no Cargo.toml defaults to hackage)",
        include = "Packages to publish (globs on crate and Cabal package names; a Cabal package's repo name serves too); with closure, every chain package they need comes first",
        exclude = "Packages to leave out (globs)",
        closure = "Publish what the included packages need too, in dependency order (default true)",
        execute = "Publish for real; without it every group's dry run runs and nothing is published (default false). Applies to a chain and to one repo on every channel"
    ))]
    #[allow(clippy::too_many_arguments)]
    pub async fn publish(
        &self,
        org: Option<String>,
        name: String,
        channel: Option<String>,
        include: Option<Vec<String>>,
        exclude: Option<Vec<String>>,
        closure: Option<bool>,
        execute: Option<bool>,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        use futures::StreamExt as _;
        let config_dir = self.config_dir.clone();
        let chain_args = |repos, include, exclude, closure: bool, execute: bool| {
            let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            crate::v5::build::chain_ops::PublishArgs {
                repos,
                include,
                exclude,
                closure,
                execute,
                token: if execute { resolve_store_secret(&config_dir, "secrets://cargo/token") } else { None },
                hackage_secret: if execute { resolve_store_secret(&config_dir, "secrets://hackage/token") } else { None },
                log_dir: self.state_dir.join("publish").join(stamp),
            }
        };
        let Some(org) = org else {
            // HYPERFORGE-23: a crate chain over the workspace `name`.
            let repos = match chain_repos(&config_dir, &name) {
                Ok(r) => r,
                Err(e) => return futures::stream::once(async move { err("not_found", e) }).boxed(),
            };
            let args = chain_args(
                repos,
                include.unwrap_or_default(),
                exclude.unwrap_or_default(),
                closure.unwrap_or(true),
                execute.unwrap_or(false),
            );
            return from_task(move |tx| crate::v5::build::chain_ops::publish(tx, args)).boxed();
        };
        // HYPERFORGE-26: one Cabal repo takes the chain's road to Hackage,
        // dry run and all, with that one package.
        if let Ok(dir) = resolve_single_repo_dir(&config_dir, &org, &name) {
            match one_repo_cabal_package(&dir, channel.as_deref()) {
                Ok(Some(package)) => {
                    let args = chain_args(
                        vec![(format!("{org}/{name}"), dir)],
                        vec![package],
                        Vec::new(),
                        false,
                        execute.unwrap_or(false),
                    );
                    return from_task(move |tx| crate::v5::build::chain_ops::publish(tx, args)).boxed();
                }
                Ok(None) => {}
                Err(e) => return futures::stream::once(async move { err("manifest_not_found", e) }).boxed(),
            }
        }
        let config_dir = config_dir.clone();
        stream! {
            let dir = match resolve_single_repo_dir(&config_dir, &org, &name) {
                Ok(d) => d, Err(e) => { yield err("not_found", e); return; }
            };
            let ch = channel.unwrap_or_else(|| "crates.io".into());
            let (secret_key, dry_run_cmd) = match ch.as_str() {
                "crates.io" => ("cargo/token", "cargo publish --dry-run --allow-dirty"),
                "npm" => ("npm/token", "npm publish --dry-run"),
                "pypi" => ("pypi/token", "twine check dist/*"),
                other => { yield err("unknown_channel", other); return; }
            };
            // HYPERFORGE-26: publishing takes --execute. Without it, the
            // channel's dry run and nothing else; no token is read.
            if !execute.unwrap_or(false) {
                let out = match exec::run_shell_env(&dir, dry_run_cmd, &[]) {
                    Ok(r) => r, Err(e) => { yield err(e.code(), e.to_string()); return; }
                };
                if out.exit_code == 0 {
                    yield BuildEvent::PublishGroup {
                        group: 1,
                        repo: format!("{org}/{name}"),
                        crates: vec![name],
                        phase: "dry_run".into(),
                        state: "ok".into(),
                        step: dry_run_cmd.into(),
                        log: String::new(),
                        tail: None,
                    };
                } else {
                    yield err("dry_run_failed", format!("exit {}: {}", out.exit_code, out.stderr));
                }
                return;
            }
            // Resolve publishing token via SecretResolver.
            let resolver = crate::v5::secrets::YamlSecretStore::new(&config_dir);
            use crate::v5::secrets::SecretResolver as _;
            let parsed = match crate::v5::secrets::SecretRef::parse(&format!("secrets://{secret_key}")) {
                Ok(r) => r, Err(e) => { yield err(e.code(), e.to_string()); return; }
            };
            let token = match resolver.resolve(&parsed) {
                Ok(t) => t,
                Err(_) => { yield err("missing_token", format!("no secret at secrets://{secret_key}")); return; }
            };
            // HYPERFORGE-23: the token reaches the child through its
            // environment, never a shell string (the process table).
            // HYPERFORGE-46: npm reads NPM_TOKEN only through an npmrc.
            let npm_auth = if ch == "npm" {
                match exec::npm_auth_file() {
                    Ok(f) => Some(f), Err(e) => { yield err(e.code(), e.to_string()); return; }
                }
            } else {
                None
            };
            let (publish_cmd, envs): (&str, Vec<(&str, String)>) = match ch.as_str() {
                "crates.io" => ("cargo publish --allow-dirty", vec![("CARGO_REGISTRY_TOKEN", token)]),
                "npm" => {
                    let config = npm_auth.as_ref().map(|f| f.path().display().to_string()).unwrap_or_default();
                    ("npm publish", vec![("NPM_TOKEN", token), ("NPM_CONFIG_USERCONFIG", config)])
                }
                "pypi" => ("twine upload dist/*", vec![("TWINE_USERNAME", "__token__".into()), ("TWINE_PASSWORD", token)]),
                _ => unreachable!(),
            };
            let out = match exec::run_shell_env(&dir, publish_cmd, &envs) {
                Ok(r) => r, Err(e) => { yield err(e.code(), e.to_string()); return; }
            };
            if out.exit_code == 0 {
                yield BuildEvent::PackagePublished {
                    reference: crate::v5::repos::RepoRefWire { org, name },
                    channel: ch,
                };
            } else {
                yield err("publish_failed", format!("exit {}: {}", out.exit_code, out.stderr));
            }
        }.boxed()
    }

    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        bump = "major | minor | patch",
        channel = "Publish after the tag lands: crates.io | npm | pypi | hackage; none publishes nothing",
        execute = "Do it; without it the release is planned and nothing is written (default false)"
    ))]
    pub async fn release(
        &self,
        org: String,
        name: String,
        bump: Option<String>,
        channel: Option<String>,
        execute: Option<bool>,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        use futures::StreamExt as _;
        let config_dir = self.config_dir.clone();
        let service = self.clone();
        stream! {
            let dir = match resolve_single_repo_dir(&config_dir, &org, &name) {
                Ok(d) => d, Err(e) => { yield err("not_found", e); return; }
            };
            // HYPERFORGE-44: everything that can be refused is, before a write.
            let secret_key = match channel.as_deref() {
                None | Some("hackage") => None,
                Some("crates.io") => Some("cargo/token"),
                Some("npm") => Some("npm/token"),
                Some("pypi") => Some("pypi/token"),
                Some(other) => { yield err("unknown_channel", other); return; }
            };
            let plan = match release::plan(&dir, bump.as_deref().unwrap_or("patch")) {
                Ok(p) => p, Err(r) => { yield err(r.code, r.message); return; }
            };
            let reference = crate::v5::repos::RepoRefWire { org: org.clone(), name: name.clone() };
            if !execute.unwrap_or(false) {
                yield BuildEvent::ReleasePlanned {
                    reference,
                    old: plan.old,
                    new: plan.new,
                    tag: plan.tag,
                    branch: plan.branch,
                    channel,
                };
                return;
            }
            if let Some(key) = secret_key {
                if resolve_store_secret(&config_dir, &format!("secrets://{key}")).is_none() {
                    yield err("missing_token", format!("no secret at secrets://{key}"));
                    return;
                }
            }
            if let Err(r) = release::commit_and_tag(&dir, &plan) {
                yield err(r.code, r.message);
                return;
            }
            yield BuildEvent::VersionBumped {
                reference: reference.clone(),
                old: plan.old.clone(),
                new: plan.new.clone(),
            };
            // `plan` proved the branch level with origin: this is the bump.
            if let Err(e) = crate::v5::ops::git::push_refs(&dir, "origin", Some(&plan.branch)) {
                yield err(e.code(), format!("{} is committed and tagged locally, not pushed: {e}", plan.tag));
                return;
            }
            if let Err(e) = crate::v5::ops::git::push_ref(&dir, "origin", &plan.tag) {
                yield err(e.code(), format!("the bump is pushed, {} is not: {e}", plan.tag));
                return;
            }
            yield BuildEvent::ReleaseCreated { reference, tag: plan.tag };
            // Publish is the publish road's, and only it says published.
            if let Some(ch) = channel {
                let mut published =
                    Box::pin(service.publish(Some(org), name, Some(ch), None, None, Some(false), Some(true)).await);
                while let Some(ev) = published.next().await {
                    yield ev;
                }
            }
        }
    }

    #[plexus_macros::method(params(
        name = "Workspace name",
        execute = "Bump, commit and tag every member, unpushed; without it each is planned and nothing is written (default false)"
    ))]
    pub async fn release_all(
        &self,
        name: String,
        execute: Option<bool>,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let members = match workspace_members(&config_dir, &name) {
                Ok(v) => v, Err(e) => { yield err("not_found", e); return; }
            };
            let executed = execute.unwrap_or(false);
            let total = u32::try_from(members.len()).unwrap_or(u32::MAX);
            let mut ok = 0u32;
            let mut errored = 0u32;
            for (r, dir) in &members {
                let plan = match release::plan(dir, "patch") {
                    Ok(p) => p,
                    Err(refused) => {
                        errored += 1;
                        yield err(refused.code, format!("{}/{}: {}", r.org, r.name, refused.message));
                        continue;
                    }
                };
                if !executed {
                    ok += 1;
                    yield BuildEvent::ReleasePlanned {
                        reference: r.clone(),
                        old: plan.old,
                        new: plan.new,
                        tag: plan.tag,
                        branch: plan.branch,
                        channel: None,
                    };
                    continue;
                }
                match release::commit_and_tag(dir, &plan) {
                    Ok(()) => {
                        ok += 1;
                        yield BuildEvent::VersionBumped { reference: r.clone(), old: plan.old, new: plan.new };
                    }
                    Err(refused) => {
                        errored += 1;
                        yield err(refused.code, format!("{}/{}: {}", r.org, r.name, refused.message));
                    }
                }
            }
            yield BuildEvent::ReleaseSummary { name, total, ok, errored, executed };
        }
    }

    // ==================================================================
    // V5PARITY-11: dist + exec.
    // ==================================================================

    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name"
    ))]
    pub async fn init_configs(
        &self,
        org: String,
        name: String,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let dir = match resolve_single_repo_dir(&config_dir, &org, &name) {
                Ok(d) => d, Err(e) => { yield err("not_found", e); return; }
            };
            match dist::init_dist_toml(&dir) {
                Ok((path, created)) => yield BuildEvent::DistInit {
                    reference: Some(crate::v5::repos::RepoRefWire { org, name }),
                    path: path.display().to_string(),
                    created,
                },
                Err(e) => yield err(e.code(), e.to_string()),
            }
        }
    }

    #[plexus_macros::method(params(path = "Repo checkout directory"))]
    pub async fn binstall_init(
        &self,
        path: String,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        stream! {
            let dir = std::path::PathBuf::from(&path);
            let cargo = dir.join("Cargo.toml");
            match dist::binstall_init(&cargo) {
                Ok(modified) => yield BuildEvent::BinstallInit {
                    path: cargo.display().to_string(),
                    modified,
                },
                Err(e) => yield err(e.code(), e.to_string()),
            }
        }
    }

    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        tap = "Homebrew tap (user/homebrew-tap)",
        url = "Release tarball URL",
        sha256 = "Tarball sha256",
        version = "Version string",
        dry_run = "If true, emit the formula content without writing"
    ))]
    pub async fn brew_formula(
        &self,
        org: String,
        name: String,
        tap: Option<String>,
        url: String,
        sha256: String,
        version: String,
        dry_run: Option<serde_json::Value>,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        let _ = org;
        stream! {
            let dry = dry_run.as_ref().is_some_and(|v| match v {
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::String(s) => matches!(s.as_str(), "true" | "1" | "yes"),
                _ => false,
            });
            let content = dist::brew_formula(&name, &version, &url, &sha256, "");
            let written_to = if dry { None } else {
                let base = tap.as_deref().unwrap_or(".");
                let out_path = std::path::PathBuf::from(base).join(format!("{name}.rb"));
                if let Some(parent) = out_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(&out_path, &content) {
                    Ok(()) => Some(out_path.display().to_string()),
                    Err(e) => { yield err("io", e.to_string()); return; }
                }
            };
            yield BuildEvent::BrewFormula {
                name, version, dry_run: dry, written_to, content,
            };
        }
    }

    #[plexus_macros::method(params(name = "Workspace name"))]
    pub async fn dist_init(
        &self,
        name: String,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let members = match workspace_members(&config_dir, &name) {
                Ok(v) => v, Err(e) => { yield err("not_found", e); return; }
            };
            for (r, dir) in members {
                match dist::init_dist_toml(&dir) {
                    Ok((path, created)) => yield BuildEvent::DistInit {
                        reference: Some(r),
                        path: path.display().to_string(),
                        created,
                    },
                    Err(e) => yield err(e.code(), e.to_string()),
                }
            }
        }
    }

    #[plexus_macros::method(params(name = "Workspace name"))]
    pub async fn dist_show(
        &self,
        name: String,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            let members = match workspace_members(&config_dir, &name) {
                Ok(v) => v, Err(e) => { yield err("not_found", e); return; }
            };
            for (r, dir) in members {
                match dist::read_dist_toml(&dir) {
                    Ok(content) => yield BuildEvent::DistShow {
                        reference: Some(r),
                        path: dir.join(".hyperforge").join("dist.toml").display().to_string(),
                        content,
                    },
                    Err(e) => yield err(e.code(), e.to_string()),
                }
            }
        }
    }

    #[plexus_macros::method(params(
        name = "Workspace name",
        cmd = "Shell command to run in each member's checkout"
    ))]
    pub async fn run(
        &self,
        name: String,
        cmd: String,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            if cmd.is_empty() {
                yield err("validation", "missing required parameter 'cmd'"); return;
            }
            let members = match workspace_members(&config_dir, &name) {
                Ok(v) => v, Err(e) => { yield err("not_found", e); return; }
            };
            let total = u32::try_from(members.len()).unwrap_or(u32::MAX);
            let mut ok = 0u32;
            let mut errored = 0u32;
            for (r, dir) in members {
                match exec::run_shell(&dir, &cmd) {
                    Ok(res) => {
                        if res.exit_code == 0 { ok += 1; } else { errored += 1; }
                        yield BuildEvent::ExecOutput {
                            reference: Some(r),
                            path: dir.display().to_string(),
                            exit_code: res.exit_code,
                            stdout: res.stdout,
                            stderr: res.stderr,
                        };
                    }
                    Err(e) => {
                        errored += 1;
                        yield err(e.code(), e.to_string());
                    }
                }
            }
            yield BuildEvent::ExecSummary { name, total, ok, errored };
        }
    }

    #[plexus_macros::method(params(
        org = "Org name",
        name = "Repo name",
        cmd = "Shell command to run in the repo's checkout"
    ))]
    pub async fn exec(
        &self,
        org: String,
        name: String,
        cmd: String,
    ) -> impl Stream<Item = BuildEvent> + Send + 'static {
        let config_dir = self.config_dir.clone();
        stream! {
            if cmd.is_empty() {
                yield err("validation", "missing required parameter 'cmd'"); return;
            }
            let dir = match resolve_single_repo_dir(&config_dir, &org, &name) {
                Ok(d) => d, Err(e) => { yield err("not_found", e); return; }
            };
            match exec::run_shell(&dir, &cmd) {
                Ok(res) => yield BuildEvent::ExecOutput {
                    reference: Some(crate::v5::repos::RepoRefWire { org, name }),
                    path: dir.display().to_string(),
                    exit_code: res.exit_code,
                    stdout: res.stdout,
                    stderr: res.stderr,
                },
                Err(e) => yield err(e.code(), e.to_string()),
            }
        }
    }
}


// Silence unused-import warnings if OrgName is imported for future refs.
#[allow(dead_code)]
fn _unused(_: OrgName) {}

//! `build::chain_ops` — HYPERFORGE-23: the I/O half of a crate chain.
//!
//! `package_diff` against the registry, `bump` across repos (one commit
//! each), and `publish` — every crate's dry run first, verified together,
//! then each group in dependency order. Each runs as a task sending its
//! events, so a long publish outlives a dropped client rather than
//! stopping half-way.
//!
//! HYPERFORGE-26: Cabal packages take the same road to Hackage, each its own
//! group; [`crate::package::hackage::HackageClient`] is the registry and the
//! `cabal` commands.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::sync::mpsc::UnboundedSender;

use super::chain::{self, Chain, CrateNode, PackageKind, Published, Registry};
use crate::package::hackage::HackageClient;
use crate::v5::build_service::BuildEvent;

type Tx = UnboundedSender<BuildEvent>;

fn send(tx: &Tx, ev: BuildEvent) {
    let _ = tx.send(ev);
}

fn err(tx: &Tx, code: &str, message: impl Into<String>) {
    send(tx, BuildEvent::Error { code: Some(code.into()), message: message.into() });
}

fn note(tx: &Tx, message: impl Into<String>) {
    send(tx, BuildEvent::ChainNote { message: message.into() });
}

fn rel(base: &Path, p: &Path) -> String {
    p.strip_prefix(base).map_or_else(
        |_| p.display().to_string(),
        |r| if r.as_os_str().is_empty() { ".".into() } else { r.display().to_string() },
    )
}

/// `git -C dir args…` → stdout, or its stderr as the error.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    crate::v5::ops::git::run_in(dir, args)
}

/// Uncommitted changes (tracked or untracked, not ignored) under `path`.
pub(crate) fn dirty(repo_dir: &Path, path: &Path) -> Result<Vec<String>, String> {
    let r = rel(repo_dir, path);
    let out = git(repo_dir, &["status", "--porcelain", "--", &r])?;
    Ok(out.lines().map(str::to_string).collect())
}

/// Each package's registry.
struct Registries {
    crates: Registry,
    hackage: HackageClient,
}

impl Registries {
    fn from_env() -> Self {
        Self { crates: Registry::from_env(), hackage: HackageClient::new() }
    }

    async fn published(&self, c: &CrateNode) -> Result<Vec<Published>, String> {
        match c.kind {
            PackageKind::Cargo => self.crates.published(&c.name).await,
            PackageKind::Cabal => {
                let v = self.hackage.versions(&c.name).await.map_err(|e| e.to_string())?;
                let held = v.held.into_iter().map(|version| Published { version, yanked: false });
                let deprecated = v.deprecated.into_iter().map(|version| Published { version, yanked: true });
                Ok(held.chain(deprecated).collect())
            }
        }
    }

    async fn download(&self, c: &CrateNode) -> Result<Option<Vec<u8>>, String> {
        match c.kind {
            PackageKind::Cargo => self.crates.download(&c.name, &c.version).await,
            PackageKind::Cabal => self.hackage.download(&c.name, &c.version).await.map_err(|e| e.to_string()),
        }
    }
}

fn discover_selected(
    tx: &Tx,
    repos: &[(String, PathBuf)],
    include: &[String],
    exclude: &[String],
    closure: bool,
) -> Option<(Chain, Vec<usize>)> {
    let chain = chain::discover(repos);
    for w in &chain.warnings {
        note(tx, w.clone());
    }
    match chain::select(&chain, include, exclude, closure) {
        Ok((sel, notes)) => {
            for n in notes {
                note(tx, n);
            }
            Some((chain, sel))
        }
        Err(e) => {
            err(tx, "validation", e);
            None
        }
    }
}

// ======================================================================
// package_diff
// ======================================================================

pub async fn package_diff(
    tx: Tx,
    repos: Vec<(String, PathBuf)>,
    include: Vec<String>,
    exclude: Vec<String>,
) {
    let include = if include.is_empty() { vec!["*".to_string()] } else { include };
    let Some((chain, sel)) = discover_selected(&tx, &repos, &include, &exclude, false) else { return };
    let registry = Registries::from_env();
    let order = chain::order(&chain, &sel).unwrap_or_else(|_| vec![sel.clone()]);
    for &i in order.iter().flatten() {
        let c = &chain.crates[i];
        let published = match registry.published(c).await {
            Ok(p) => p,
            Err(e) => {
                err(&tx, "registry", format!("{}: {e}", c.name));
                continue;
            }
        };
        let (mut status, latest) = match c.kind {
            PackageKind::Cargo => chain::registry_status(&c.version, &published),
            PackageKind::Cabal => chain::pvp_status(&c.version, &published),
        };
        let mut changed = Vec::new();
        let mut published_from = None;
        if status == "published" || status == "yanked" {
            match registry.download(c).await {
                Ok(Some(bytes)) => match chain::drift(&bytes, &c.dir) {
                    Ok(d) => {
                        if !d.changed.is_empty() && status == "published" {
                            status = "drifted";
                        }
                        changed = d.changed;
                        published_from = d.published_from;
                    }
                    Err(e) => note(&tx, format!("{}: could not read its package: {e}", c.name)),
                },
                Ok(None) => note(&tx, format!("{} {}: no package to compare", c.name, c.version)),
                Err(e) => note(&tx, format!("{}: {e}", c.name)),
            }
        }
        let changed_count = u32::try_from(changed.len()).unwrap_or(u32::MAX);
        changed.truncate(25);
        send(
            &tx,
            BuildEvent::CrateState {
                name: c.name.clone(),
                version: c.version.clone(),
                repo: c.repo.clone(),
                path: c.dir.display().to_string(),
                status: status.into(),
                latest,
                changed_count,
                changed,
                published_from,
                publish: c.publish,
            },
        );
    }
}

// ======================================================================
// bump
// ======================================================================

pub struct BumpArgs {
    pub repos: Vec<(String, PathBuf)>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub closure: bool,
    pub kind_or_target: String,
    pub dry_run: bool,
    pub commit: bool,
}

pub async fn bump(tx: Tx, a: BumpArgs) {
    struct Edit {
        repo: String,
        repo_dir: PathBuf,
        manifest: PathBuf,
        text: String,
    }
    let Some((chain, sel)) = discover_selected(&tx, &a.repos, &a.include, &a.exclude, a.closure) else { return };
    // HYPERFORGE-26: a .cabal file is not bumped here. Asked for outright that
    // is refused; caught by a glob beside cargo crates, it is named and left.
    let (cabal, sel): (Vec<usize>, Vec<usize>) = sel.into_iter().partition(|&i| chain.crates[i].kind == PackageKind::Cabal);
    if !cabal.is_empty() {
        let names: Vec<&str> = cabal.iter().map(|&i| chain.crates[i].name.as_str()).collect();
        let message = format!("{}: bump does not edit .cabal files; set its version by hand", names.join(", "));
        if sel.is_empty() {
            return err(&tx, "unsupported", message);
        }
        note(&tx, message);
    }
    // New versions.
    let mut bumped: HashMap<String, (String, String)> = HashMap::new();
    let mut set_manifests: HashMap<PathBuf, (usize, String)> = HashMap::new();
    for &i in &sel {
        let c = &chain.crates[i];
        if c.version_inherited {
            err(&tx, "unsupported", format!("{} inherits its version from [workspace.package]; bump that workspace's version by hand", c.name));
            return;
        }
        match chain::next_version(&c.version, &a.kind_or_target) {
            Ok(v) => {
                bumped.insert(c.name.clone(), (c.version.clone(), v.clone()));
                set_manifests.insert(c.manifest.clone(), (i, v));
            }
            Err(e) => {
                err(&tx, "not_semver", format!("{}: {e}", c.name));
                return;
            }
        }
    }
    // Every manifest: its own version if bumped, its requirements on
    // bumped crates.
    let mut edits: Vec<Edit> = Vec::new();
    let mut events: Vec<BuildEvent> = Vec::new();
    for (repo, repo_dir, manifest) in &chain.manifests {
        let Ok(raw) = std::fs::read_to_string(manifest) else { continue };
        let Ok(mut doc) = raw.parse::<toml_edit::DocumentMut>() else {
            note(&tx, format!("{}: not editable TOML — skipped", manifest.display()));
            continue;
        };
        let in_set = set_manifests.get(manifest);
        if let Some((i, new)) = in_set {
            match chain::set_package_version(&mut doc, new) {
                Ok(old) => events.push(BuildEvent::CrateBumped {
                    name: chain.crates[*i].name.clone(),
                    repo: repo.clone(),
                    old,
                    new: new.clone(),
                    dry_run: a.dry_run,
                }),
                Err(e) => {
                    err(&tx, "manifest_parse_error", format!("{}: {e}", manifest.display()));
                    return;
                }
            }
        }
        // A workspace root holding bumped members is in the set too.
        let root_of_set = sel.iter().any(|&i| chain.crates[i].ws_root.join("Cargo.toml") == *manifest);
        for ch in chain::update_requirements(&mut doc, &bumped, in_set.is_some() || root_of_set) {
            events.push(BuildEvent::RequirementBumped {
                manifest: manifest.display().to_string(),
                repo: repo.clone(),
                key: ch.key,
                package: ch.package,
                table: ch.table,
                old: ch.old,
                new: ch.new,
                dry_run: a.dry_run,
            });
        }
        let text = doc.to_string();
        if text != raw {
            edits.push(Edit { repo: repo.clone(), repo_dir: repo_dir.clone(), manifest: manifest.clone(), text });
        }
    }
    // Nothing is written over someone's uncommitted change.
    let mut refused = Vec::new();
    for e in &edits {
        match dirty(&e.repo_dir, &e.manifest) {
            Ok(d) if !d.is_empty() => refused.push(format!("{} has uncommitted changes", e.manifest.display())),
            Ok(_) => {}
            Err(x) => refused.push(format!("{}: {x}", e.manifest.display())),
        }
    }
    if !refused.is_empty() && !a.dry_run {
        err(&tx, "dirty", format!("nothing written: {}", refused.join("; ")));
        return;
    }
    for ev in events {
        send(&tx, ev);
    }
    if a.dry_run {
        for r in refused {
            note(&tx, format!("would refuse: {r}"));
        }
        return;
    }
    for e in &edits {
        if let Err(x) = std::fs::write(&e.manifest, &e.text) {
            err(&tx, "io", format!("{}: {x}", e.manifest.display()));
            return;
        }
    }
    // One commit per repo: its manifests, and its lockfile brought along.
    let mut by_repo: BTreeMap<(String, PathBuf), Vec<PathBuf>> = BTreeMap::new();
    for e in &edits {
        by_repo.entry((e.repo.clone(), e.repo_dir.clone())).or_default().push(e.manifest.clone());
    }
    for ((repo, repo_dir), manifests) in by_repo {
        let mut files: Vec<String> = manifests.iter().map(|m| rel(&repo_dir, m)).collect();
        match refresh_lock(&repo_dir).await {
            Ok(true) => files.push("Cargo.lock".into()),
            Ok(false) => {}
            Err(x) => note(&tx, format!("{repo}: Cargo.lock not refreshed: {x}")),
        }
        if !a.commit {
            send(&tx, BuildEvent::BumpCommitted { repo, path: repo_dir.display().to_string(), commit: None, files });
            continue;
        }
        let in_repo: Vec<String> = sel
            .iter()
            .map(|&i| &chain.crates[i])
            .filter(|c| c.repo_dir == repo_dir)
            .map(|c| format!("{} {} → {}", c.name, c.version, bumped[&c.name].1))
            .collect();
        let msg = if in_repo.is_empty() {
            let mut reqs: Vec<String> = bumped.iter().map(|(n, (_, v))| format!("{n} {v}")).collect();
            reqs.sort();
            format!("chore(deps): requirements on {} (hyperforge build bump)", reqs.join(", "))
        } else {
            format!("chore(release): {} (hyperforge build bump)", in_repo.join(", "))
        };
        let mut args: Vec<&str> = vec!["commit", "-q", "-m", &msg, "--"];
        args.extend(files.iter().map(String::as_str));
        match git(&repo_dir, &args).and_then(|_| git(&repo_dir, &["rev-parse", "--short", "HEAD"])) {
            Ok(h) => send(&tx, BuildEvent::BumpCommitted {
                repo,
                path: repo_dir.display().to_string(),
                commit: Some(h.trim().to_string()),
                files,
            }),
            Err(x) => {
                err(&tx, "git", format!("{repo}: commit failed (manifests are written): {x}"));
                return;
            }
        }
    }
}

// ======================================================================
// link
// ======================================================================

const LINK_MARK: &str = "# Written by hyperforge build link; `build unlink` removes it.";
const LOCK_CLEAN: &str = "# Cargo.lock was clean at link.";

/// Add a line to a checkout's own `info/exclude`, so nothing tracked changes.
fn exclude_locally(dir: &Path, line: &str) -> Result<(), String> {
    let rel_path = git(dir, &["rev-parse", "--git-path", "info/exclude"])?;
    let path = dir.join(rel_path.trim());
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    if have.lines().any(|l| l == line) {
        return Ok(());
    }
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, format!("{have}{line}\n")).map_err(|e| e.to_string())
}

/// Point one checkout's cargo at sibling checkouts of the included crates,
/// through a `.cargo/config.toml` git ignores. The checkout's manifests stay
/// as published.
pub async fn link(tx: Tx, repos: Vec<(String, PathBuf)>, path: PathBuf, include: Vec<String>, exclude: Vec<String>) {
    let Some((chain, sel)) = discover_selected(&tx, &repos, &include, &exclude, false) else { return };
    let here = path.canonicalize().unwrap_or_else(|_| path.clone());
    let entries: Vec<(String, PathBuf)> = sel
        .iter()
        .map(|&i| &chain.crates[i])
        .filter(|c| c.kind == PackageKind::Cargo && c.repo_dir.canonicalize().map_or(true, |d| d != here))
        .map(|c| (c.name.clone(), c.dir.clone()))
        .collect();
    if entries.is_empty() {
        return err(&tx, "validation", "no crates to link: the include matched nothing outside this checkout");
    }
    let config = path.join(".cargo/config.toml");
    let ours = std::fs::read_to_string(&config).ok().is_none_or(|s| s.starts_with(LINK_MARK));
    if !ours {
        return err(&tx, "conflict", format!("{} exists and is not build link's", config.display()));
    }
    if git(&path, &["ls-files", "--error-unmatch", ".cargo/config.toml"]).is_ok() {
        return err(&tx, "conflict", format!("{} is tracked", config.display()));
    }
    let lock = path.join("Cargo.lock");
    let clean = git(&path, &["ls-files", "--error-unmatch", "Cargo.lock"]).is_ok()
        && dirty(&path, &lock).is_ok_and(|d| d.is_empty());
    let mut text = format!("{LINK_MARK}\n");
    if clean {
        text.push_str(LOCK_CLEAN);
        text.push('\n');
    }
    text.push_str(&chain::patch_config(&entries));
    let done = std::fs::create_dir_all(path.join(".cargo"))
        .map_err(|e| e.to_string())
        .and_then(|()| std::fs::write(&config, text).map_err(|e| e.to_string()))
        .and_then(|()| exclude_locally(&path, ".cargo/config.toml"));
    if let Err(e) = done {
        return err(&tx, "io", format!("{}: {e}", config.display()));
    }
    note(&tx, "Cargo.lock will move while linked; `build unlink` restores it if it was clean.");
    send(&tx, BuildEvent::Linked {
        path: path.display().to_string(),
        crates: entries.into_iter().map(|(n, _)| n).collect(),
    });
}

/// Undo [`link`]: its config goes, and a lockfile that was clean comes back.
pub async fn unlink(tx: Tx, path: PathBuf) {
    let config = path.join(".cargo/config.toml");
    let Ok(text) = std::fs::read_to_string(&config) else {
        return err(&tx, "not_found", format!("{} is not there", config.display()));
    };
    if !text.starts_with(LINK_MARK) {
        return err(&tx, "conflict", format!("{} is not build link's", config.display()));
    }
    if let Err(e) = std::fs::remove_file(&config) {
        return err(&tx, "io", e.to_string());
    }
    let _ = std::fs::remove_dir(path.join(".cargo"));
    if text.contains(LOCK_CLEAN) {
        if let Err(e) = git(&path, &["checkout", "--", "Cargo.lock"]) {
            note(&tx, format!("Cargo.lock not restored: {e}"));
        }
    }
    send(&tx, BuildEvent::Unlinked { path: path.display().to_string() });
}

/// Bring a repo's `Cargo.lock` up with its manifests. True when a tracked
/// lockfile that was clean changed, so it belongs in the bump's commit.
pub(crate) async fn refresh_lock(repo_dir: &Path) -> Result<bool, String> {
    let lock = repo_dir.join("Cargo.lock");
    if !lock.is_file() {
        return Ok(false);
    }
    let was_clean = dirty(repo_dir, &lock).is_ok_and(|d| d.is_empty());
    let out = tokio::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(repo_dir.join("Cargo.toml"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|x| x.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("").to_string());
    }
    let tracked = git(repo_dir, &["ls-files", "--error-unmatch", "Cargo.lock"]).is_ok();
    Ok(tracked && was_clean && dirty(repo_dir, &lock).is_ok_and(|d| !d.is_empty()))
}

// ======================================================================
// publish
// ======================================================================

pub struct PublishArgs {
    pub repos: Vec<(String, PathBuf)>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub closure: bool,
    pub execute: bool,
    pub token: Option<String>,
    /// What the secret store holds at `hackage/token`.
    pub hackage_secret: Option<String>,
    pub log_dir: PathBuf,
}

struct Group {
    n: u32,
    kind: PackageKind,
    repo: String,
    repo_dir: PathBuf,
    ws_root: PathBuf,
    crates: Vec<usize>, // to publish (skips left out)
    custom: Option<chain::PublishStep>,
}

pub async fn publish(tx: Tx, a: PublishArgs) {
    let Some((chain, sel)) = discover_selected(&tx, &a.repos, &a.include, &a.exclude, a.closure) else { return };
    let order = match chain::order(&chain, &sel) {
        Ok(o) => o,
        Err(e) => return err(&tx, "cycle", e),
    };
    // The plan: a version its registry holds is skipped.
    let registry = Registries::from_env();
    let mut groups: Vec<Group> = Vec::new();
    let mut position = 0u32;
    let mut skipped = 0u32;
    for (gi, g) in order.iter().enumerate() {
        let first = &chain.crates[g[0]];
        let mut group = Group {
            n: u32::try_from(gi + 1).unwrap_or(u32::MAX),
            kind: first.kind,
            repo: first.repo.clone(),
            repo_dir: first.repo_dir.clone(),
            ws_root: first.ws_root.clone(),
            crates: Vec::new(),
            custom: None,
        };
        for &i in g {
            let c = &chain.crates[i];
            position += 1;
            let (action, reason) = match registry.published(c).await {
                Ok(p) if p.iter().any(|v| v.version == c.version) => {
                    skipped += 1;
                    ("skip", Some(format!("{} holds {} {}", c.kind.registry(), c.name, c.version)))
                }
                Ok(_) => {
                    group.crates.push(i);
                    ("publish", None)
                }
                Err(e) => return err(&tx, "registry", format!("{}: {e}", c.name)),
            };
            send(&tx, BuildEvent::PublishPlanned {
                position,
                group: group.n,
                name: c.name.clone(),
                version: c.version.clone(),
                repo: c.repo.clone(),
                action: action.into(),
                reason,
            });
        }
        if group.crates.is_empty() {
            continue;
        }
        if group.kind == PackageKind::Cabal {
            groups.push(group);
            continue;
        }
        match chain::read_publish_step(&group.repo_dir) {
            Ok(Some(step)) => {
                let names: Vec<&str> = group.crates.iter().map(|&i| chain.crates[i].name.as_str()).collect();
                let covered = |n: &str| step.crates.is_empty() || step.crates.iter().any(|c| c == n);
                if names.iter().all(|n| covered(n)) {
                    group.custom = Some(step);
                } else if names.iter().any(|n| covered(n)) {
                    return err(&tx, "validation", format!(
                        "{}: [publish] covers some of {} but not all; list them all in its crates, or none",
                        group.repo, names.join(", ")
                    ));
                }
            }
            Ok(None) => {}
            Err(e) => return err(&tx, "config", e),
        }
        groups.push(group);
    }
    let planned = u32::try_from(groups.iter().map(|g| g.crates.len()).sum::<usize>()).unwrap_or(u32::MAX);
    if groups.is_empty() {
        note(&tx, "every selected package's version is on its registry already — bump first");
        return send(&tx, summary(a.execute, planned, skipped, 0, vec![], vec![], vec![]));
    }
    // An execute publishes commits, never a working tree.
    if a.execute {
        let mut refused = Vec::new();
        for g in &groups {
            for &i in &g.crates {
                let c = &chain.crates[i];
                match dirty(&c.repo_dir, &c.dir) {
                    Ok(d) if !d.is_empty() => refused.push(format!("{} ({}): {}", c.name, c.repo, d.join(", "))),
                    Ok(_) => {}
                    Err(e) => refused.push(format!("{}: {e}", c.name)),
                }
            }
        }
        if !refused.is_empty() {
            return err(&tx, "dirty", format!("nothing published — uncommitted changes: {}", refused.join("; ")));
        }
    }
    if let Err(e) = std::fs::create_dir_all(&a.log_dir) {
        return err(&tx, "io", format!("{}: {e}", a.log_dir.display()));
    }
    // 1. Every group's dry run, each against the others' checkouts.
    let to_publish: Vec<(String, PathBuf, PathBuf, PackageKind)> = groups
        .iter()
        .flat_map(|g| g.crates.iter().map(|&i| {
            let c = &chain.crates[i];
            (c.name.clone(), c.dir.clone(), c.ws_root.clone(), c.kind)
        }))
        .collect();
    let mut verified = 0u32;
    let mut failed: Vec<String> = Vec::new();
    for g in &groups {
        let patch: Vec<(String, PathBuf)> = to_publish
            .iter()
            .filter(|(_, _, ws, kind)| *kind == PackageKind::Cargo && *ws != g.ws_root)
            .map(|(n, d, _, _)| (n.clone(), d.clone()))
            .collect();
        let patch_file = if patch.is_empty() || g.kind == PackageKind::Cabal {
            None
        } else {
            let p = a.log_dir.join(format!("{:02}-patch.toml", g.n));
            if let Err(e) = std::fs::write(&p, chain::patch_config(&patch)) {
                return err(&tx, "io", e.to_string());
            }
            Some(p)
        };
        if run_group(&tx, &chain, g, false, patch_file.as_deref(), None, &a.log_dir).await {
            verified += u32::try_from(g.crates.len()).unwrap_or(0);
        } else {
            failed.extend(g.crates.iter().map(|&i| chain.crates[i].name.clone()));
        }
    }
    let all_names: Vec<String> = to_publish.iter().map(|(n, ..)| n.clone()).collect();
    if !failed.is_empty() {
        err(&tx, "dry_run_failed", format!("nothing published: the dry run failed for {}", failed.join(", ")));
        return send(&tx, summary(a.execute, planned, skipped, verified, vec![], failed, all_names));
    }
    if !a.execute {
        return send(&tx, summary(false, planned, skipped, verified, vec![], vec![], all_names));
    }
    // 2. Publish, group by group, in order; stop at the first failure.
    let has = |kind| groups.iter().any(|g| g.kind == kind);
    if a.token.is_none() && has(PackageKind::Cargo) {
        note(&tx, "no secrets://cargo/token — cargo uses its own credentials");
    }
    let hackage_token = if has(PackageKind::Cabal) {
        registry.hackage.resolve_token_with(a.hackage_secret.clone()).await
    } else {
        None
    };
    if hackage_token.is_none() && has(PackageKind::Cabal) {
        note(&tx, "no Hackage token (HACKAGE_TOKEN, secrets://hackage/token, password-command) — cabal uses its own credentials");
    }
    let mut published: Vec<String> = Vec::new();
    for g in &groups {
        let token = match g.kind {
            PackageKind::Cargo => a.token.as_deref(),
            PackageKind::Cabal => hackage_token.as_deref(),
        };
        if !run_group(&tx, &chain, g, true, None, token, &a.log_dir).await {
            let names: Vec<String> = g.crates.iter().map(|&i| chain.crates[i].name.clone()).collect();
            let rest: Vec<String> = all_names.iter().filter(|n| !published.contains(n)).cloned().collect();
            let code = if published.is_empty() { "publish_failed" } else { "half_published" };
            err(&tx, code, format!(
                "publishing {} failed; published: [{}]; not published: [{}]",
                names.join(", "), published.join(", "), rest.join(", ")
            ));
            return send(&tx, summary(true, planned, skipped, verified, published, names, rest));
        }
        for &i in &g.crates {
            let c = &chain.crates[i];
            let on_registry = registry
                .published(c)
                .await
                .is_ok_and(|p| p.iter().any(|v| v.version == c.version));
            if !on_registry {
                note(&tx, format!("{} {}: not in the index yet", c.name, c.version));
            }
            published.push(c.name.clone());
            let tag = format!("{}-v{}", c.name, c.version);
            let msg = format!("Release {} v{} (hyperforge build publish)", c.name, c.version);
            match git(&c.repo_dir, &["tag", "-a", &tag, "-m", &msg]) {
                Ok(_) => send(&tx, BuildEvent::CrateTagged { name: c.name.clone(), repo: c.repo.clone(), tag }),
                Err(e) => note(&tx, format!("{}: tag {tag} not made: {e}", c.name)),
            }
        }
    }
    send(&tx, summary(true, planned, skipped, verified, published, vec![], vec![]));
}

/// A cargo-side command's environment: the token on execute, else none.
fn cargo_env(mut cmd: tokio::process::Command, execute: bool, token: Option<&str>) -> tokio::process::Command {
    if execute {
        if let Some(t) = token {
            cmd.env("CARGO_REGISTRY_TOKEN", t);
        }
    } else {
        cmd.env_remove("CARGO_REGISTRY_TOKEN");
    }
    cmd.env("CARGO_TERM_COLOR", "never");
    cmd
}

const fn summary(
    execute: bool,
    planned: u32,
    skipped: u32,
    verified: u32,
    published: Vec<String>,
    failed: Vec<String>,
    not_published: Vec<String>,
) -> BuildEvent {
    BuildEvent::PublishSummary { execute, planned, skipped, verified, published, failed, not_published }
}

/// A Cabal group's commands. The dry run is `cabal check` then `cabal sdist`
/// into the run's dir; the publish uploads that tarball. Returns the
/// commands, what to show for them, the tarball the phase must leave, and
/// the private config holding the token, alive as long as the commands.
fn cabal_steps(
    c: &CrateNode,
    execute: bool,
    token: Option<&str>,
    log_dir: &Path,
) -> std::io::Result<(Vec<tokio::process::Command>, String, PathBuf, Option<tempfile::NamedTempFile>)> {
    let out = log_dir.join("sdist");
    let tarball = out.join(format!("{}-{}.tar.gz", c.name, c.version));
    if execute {
        let config = token.map(HackageClient::token_config).transpose()?;
        let upload = HackageClient::upload_command(
            &c.dir,
            &tarball.display().to_string(),
            config.as_ref().map(tempfile::NamedTempFile::path),
        );
        return Ok((vec![upload], format!("cabal upload --publish {}", tarball.display()), tarball, config));
    }
    let cmds = vec![HackageClient::check_command(&c.dir), HackageClient::sdist_command(&c.dir, Some(&out))];
    Ok((cmds, format!("cabal check && cabal sdist --ignore-project -o {}", out.display()), tarball, None))
}

/// One group's dry run or publish: `cargo publish -p …` in its cargo
/// workspace, the repo's own `[publish]` step, or a Cabal package's
/// [`cabal_steps`]. Output goes to a log under the run's dir; the event
/// carries its path and, on failure, its tail. The token reaches the child
/// through its environment (cargo) or a private config (cabal) only.
async fn run_group(
    tx: &Tx,
    chain: &Chain,
    g: &Group,
    execute: bool,
    patch_file: Option<&Path>,
    token: Option<&str>,
    log_dir: &Path,
) -> bool {
    let phase = if execute { "execute" } else { "dry_run" };
    let names: Vec<String> = g.crates.iter().map(|&i| chain.crates[i].name.clone()).collect();
    let repo_short = g.repo.rsplit('/').next().unwrap_or(&g.repo).to_string();
    let log = log_dir.join(format!("{:02}-{repo_short}-{phase}.log", g.n));
    let cabal = if g.kind == PackageKind::Cabal {
        match cabal_steps(&chain.crates[g.crates[0]], execute, token, log_dir) {
            Ok(steps) => Some(steps),
            Err(e) => {
                err(tx, "io", format!("{}: {e}", names.join(", ")));
                return false;
            }
        }
    } else {
        None
    };
    let (cmds, step, tarball, _token_config) = if let Some((cmds, step, tarball, config)) = cabal {
        (cmds, step, Some(tarball), config)
    } else if let Some(step) = &g.custom {
        let line = if execute { &step.execute } else { &step.dry_run };
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(line).current_dir(&g.repo_dir);
        c.env("HF_PUBLISH_CRATES", names.join(" "));
        c.env("HF_PUBLISH_MODE", if execute { "execute" } else { "dry-run" });
        if let Some(p) = patch_file {
            c.env("HF_CARGO_CONFIG", p);
        }
        (vec![cargo_env(c, execute, token)], format!("custom: {line}"), None, None)
    } else {
        let mut c = tokio::process::Command::new("cargo");
        c.arg("publish").current_dir(&g.ws_root);
        if !execute {
            c.args(["--dry-run", "--allow-dirty"]);
        }
        for n in &names {
            c.args(["-p", n]);
        }
        if let Some(p) = patch_file {
            c.arg("--config").arg(p);
        }
        let shown = format!(
            "cargo publish{} {}{}",
            if execute { "" } else { " --dry-run --allow-dirty" },
            names.iter().map(|n| format!("-p {n}")).collect::<Vec<_>>().join(" "),
            patch_file.map(|p| format!(" --config {}", p.display())).unwrap_or_default()
        );
        (vec![cargo_env(c, execute, token)], shown, None, None)
    };
    let file = match std::fs::File::create(&log) {
        Ok(f) => f,
        Err(e) => {
            err(tx, "io", format!("{}: {e}", log.display()));
            return false;
        }
    };
    let event = |state: &str, tail: Option<String>| BuildEvent::PublishGroup {
        group: g.n,
        repo: g.repo.clone(),
        crates: names.clone(),
        phase: phase.into(),
        state: state.into(),
        step: step.clone(),
        log: log.display().to_string(),
        tail,
    };
    send(tx, event("started", None));
    let mut ok = true;
    for mut cmd in cmds {
        let (Ok(out), Ok(errf)) = (file.try_clone(), file.try_clone()) else {
            err(tx, "io", "log file");
            return false;
        };
        cmd.stdin(Stdio::null()).stdout(Stdio::from(out)).stderr(Stdio::from(errf));
        ok = match cmd.status().await {
            Ok(s) => s.success(),
            Err(e) => {
                let _ = std::fs::write(&log, format!("could not start: {e}\n"));
                false
            }
        };
        if !ok {
            break;
        }
    }
    if let Some(t) = tarball.filter(|t| ok && !t.is_file()) {
        let _ = std::fs::write(&log, format!("no tarball at {}\n", t.display()));
        ok = false;
    }
    if ok && !execute && g.kind == PackageKind::Cabal {
        let warnings = std::fs::read_to_string(&log)
            .map_or(0, |s| s.lines().filter(|l| l.starts_with("Warning:")).count());
        if warnings > 0 {
            note(tx, format!("{}: cabal check: {warnings} warnings (not failures); see {}", names.join(", "), log.display()));
        }
    }
    let tail = if ok {
        None
    } else {
        std::fs::read_to_string(&log).ok().map(|s| {
            let lines: Vec<&str> = s.lines().collect();
            lines[lines.len().saturating_sub(30)..].join("\n")
        })
    };
    send(tx, event(if ok { "ok" } else { "failed" }, tail));
    ok
}

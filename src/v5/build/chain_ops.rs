//! `build::chain_ops` — HYPERFORGE-23: the I/O half of a crate chain.
//!
//! `package_diff` against the registry, `bump` across repos (one commit
//! each), and `publish` — every crate's dry run first, verified together,
//! then each group in dependency order. Each runs as a task sending its
//! events, so a long publish outlives a dropped client rather than
//! stopping half-way.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::sync::mpsc::UnboundedSender;

use super::chain::{self, Chain, Registry};
use crate::v5::build_hub::BuildEvent;

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
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Uncommitted changes (tracked or untracked, not ignored) under `path`.
fn dirty(repo_dir: &Path, path: &Path) -> Result<Vec<String>, String> {
    let r = rel(repo_dir, path);
    let out = git(repo_dir, &["status", "--porcelain", "--", &r])?;
    Ok(out.lines().map(str::to_string).collect())
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
    let registry = Registry::from_env();
    let order = chain::order(&chain, &sel).unwrap_or_else(|_| vec![sel.clone()]);
    for &i in order.iter().flatten() {
        let c = &chain.crates[i];
        let published = match registry.published(&c.name).await {
            Ok(p) => p,
            Err(e) => {
                err(&tx, "registry", format!("{}: {e}", c.name));
                continue;
            }
        };
        let (mut status, latest) = chain::registry_status(&c.version, &published);
        let mut changed = Vec::new();
        let mut published_from = None;
        if status == "published" || status == "yanked" {
            match registry.download(&c.name, &c.version).await {
                Ok(Some(bytes)) => match chain::drift(&bytes, &c.dir) {
                    Ok(d) => {
                        if !d.changed.is_empty() && status == "published" {
                            status = "drifted";
                        }
                        changed = d.changed;
                        published_from = d.published_from;
                    }
                    Err(e) => note(&tx, format!("{}: could not read its .crate: {e}", c.name)),
                },
                Ok(None) => note(&tx, format!("{} {}: no .crate to compare", c.name, c.version)),
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
        let lock = repo_dir.join("Cargo.lock");
        if lock.is_file() {
            let lock_was_clean = dirty(&repo_dir, &lock).is_ok_and(|d| d.is_empty());
            let out = tokio::process::Command::new("cargo")
                .args(["metadata", "--format-version", "1", "--manifest-path"])
                .arg(repo_dir.join("Cargo.toml"))
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .await;
            match out {
                Ok(o) if o.status.success() => {
                    let tracked = git(&repo_dir, &["ls-files", "--error-unmatch", "Cargo.lock"]).is_ok();
                    if tracked && lock_was_clean && dirty(&repo_dir, &lock).is_ok_and(|d| !d.is_empty()) {
                        files.push("Cargo.lock".into());
                    }
                }
                Ok(o) => note(&tx, format!("{repo}: Cargo.lock not refreshed: {}", String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or(""))),
                Err(x) => note(&tx, format!("{repo}: Cargo.lock not refreshed: {x}")),
            }
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
// publish
// ======================================================================

pub struct PublishArgs {
    pub repos: Vec<(String, PathBuf)>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub closure: bool,
    pub execute: bool,
    pub token: Option<String>,
    pub log_dir: PathBuf,
}

struct Group {
    n: u32,
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
    // The plan: a version crates.io holds is skipped.
    let registry = Registry::from_env();
    let mut groups: Vec<Group> = Vec::new();
    let mut position = 0u32;
    let mut skipped = 0u32;
    for (gi, g) in order.iter().enumerate() {
        let first = &chain.crates[g[0]];
        let mut group = Group {
            n: u32::try_from(gi + 1).unwrap_or(u32::MAX),
            repo: first.repo.clone(),
            repo_dir: first.repo_dir.clone(),
            ws_root: first.ws_root.clone(),
            crates: Vec::new(),
            custom: None,
        };
        for &i in g {
            let c = &chain.crates[i];
            position += 1;
            let (action, reason) = match registry.published(&c.name).await {
                Ok(p) if p.iter().any(|v| v.version == c.version) => {
                    skipped += 1;
                    ("skip", Some(format!("crates.io holds {} {}", c.name, c.version)))
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
        note(&tx, "every selected crate's version is on crates.io already — bump first");
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
    let to_publish: Vec<(String, PathBuf, PathBuf)> = groups
        .iter()
        .flat_map(|g| g.crates.iter().map(|&i| {
            let c = &chain.crates[i];
            (c.name.clone(), c.dir.clone(), c.ws_root.clone())
        }))
        .collect();
    let mut verified = 0u32;
    let mut failed: Vec<String> = Vec::new();
    for g in &groups {
        let patch: Vec<(String, PathBuf)> = to_publish
            .iter()
            .filter(|(_, _, ws)| *ws != g.ws_root)
            .map(|(n, d, _)| (n.clone(), d.clone()))
            .collect();
        let patch_file = if patch.is_empty() {
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
    let all_names: Vec<String> = to_publish.iter().map(|(n, _, _)| n.clone()).collect();
    if !failed.is_empty() {
        err(&tx, "dry_run_failed", format!("nothing published: the dry run failed for {}", failed.join(", ")));
        return send(&tx, summary(a.execute, planned, skipped, verified, vec![], failed, all_names));
    }
    if !a.execute {
        return send(&tx, summary(false, planned, skipped, verified, vec![], vec![], all_names));
    }
    // 2. Publish, group by group, in order; stop at the first failure.
    if a.token.is_none() {
        note(&tx, "no secrets://cargo/token — cargo uses its own credentials");
    }
    let mut published: Vec<String> = Vec::new();
    for g in &groups {
        if !run_group(&tx, &chain, g, true, None, a.token.as_deref(), &a.log_dir).await {
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
                .published(&c.name)
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

/// One group's dry run or publish: `cargo publish -p …` in its cargo
/// workspace, or the repo's own `[publish]` step. Output goes to a log
/// under the run's dir; the event carries its path and, on failure, its
/// tail. The token reaches the child through its environment only.
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
    let (mut cmd, step) = if let Some(step) = &g.custom {
        let line = if execute { &step.execute } else { &step.dry_run };
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(line).current_dir(&g.repo_dir);
        c.env("HF_PUBLISH_CRATES", names.join(" "));
        c.env("HF_PUBLISH_MODE", if execute { "execute" } else { "dry-run" });
        if let Some(p) = patch_file {
            c.env("HF_CARGO_CONFIG", p);
        }
        (c, format!("custom: {line}"))
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
        (c, shown)
    };
    if execute {
        if let Some(t) = token {
            cmd.env("CARGO_REGISTRY_TOKEN", t);
        }
    } else {
        cmd.env_remove("CARGO_REGISTRY_TOKEN");
    }
    cmd.env("CARGO_TERM_COLOR", "never").stdin(Stdio::null());
    let file = match std::fs::File::create(&log) {
        Ok(f) => f,
        Err(e) => {
            err(tx, "io", format!("{}: {e}", log.display()));
            return false;
        }
    };
    let (Ok(out), Ok(errf)) = (file.try_clone(), file.try_clone()) else {
        err(tx, "io", "log file");
        return false;
    };
    cmd.stdout(Stdio::from(out)).stderr(Stdio::from(errf));
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
    let ok = match cmd.status().await {
        Ok(s) => s.success(),
        Err(e) => {
            let _ = std::fs::write(&log, format!("could not start: {e}\n"));
            false
        }
    };
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

//! `build::chain` — HYPERFORGE-23: a crate chain across a workspace's repos.
//! HYPERFORGE-26: a repo's root `.cabal` package is in the chain too.
//!
//! Discovery (each repo's cargo workspace and its members), dependency
//! order, crates.io's state of each crate, bumps that carry every
//! dependent's `version =` requirement, and the publish plan.
//!
//! Pure where it can be: discovery reads manifests, the plan and the edits
//! are functions of them; the registry is reached through [`Registry`],
//! whose base URLs may be `file://` directories (a throwaway registry for
//! tests, `HYPERFORGE_CRATES_INDEX` / `HYPERFORGE_CRATES_DL`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ======================================================================
// Discovery.
// ======================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepKind {
    Normal,
    Build,
    Dev,
}

/// One dependency entry of a crate, by the package it names.
#[derive(Debug, Clone)]
pub struct DepRef {
    pub package: String,
    pub kind: DepKind,
    /// The `version =` requirement, if the entry has one.
    pub req: Option<String>,
}

/// Which registry a chain package belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    /// crates.io, by `cargo publish`.
    Cargo,
    /// Hackage, by `cabal upload --publish`.
    Cabal,
}

impl PackageKind {
    /// The registry's name, as an operator reads it.
    #[must_use]
    pub const fn registry(self) -> &'static str {
        match self {
            Self::Cargo => "crates.io",
            Self::Cabal => "Hackage",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CrateNode {
    pub kind: PackageKind,
    pub name: String,
    pub version: String,
    /// The version comes from `[workspace.package]` (`version.workspace = true`).
    pub version_inherited: bool,
    /// The crate's directory (holds its Cargo.toml).
    pub dir: PathBuf,
    pub manifest: PathBuf,
    /// `org/name` of the repo it lives in.
    pub repo: String,
    pub repo_dir: PathBuf,
    /// The cargo workspace root it belongs to (its own dir when standalone).
    pub ws_root: PathBuf,
    /// `publish = false` (or `publish = []`) is false.
    pub publish: bool,
    pub deps: Vec<DepRef>,
}

impl CrateNode {
    /// The repo's own name: `plexus-synapse` lives in `hypermemetic/synapse`.
    #[must_use]
    pub fn repo_name(&self) -> &str {
        self.repo.rsplit('/').next().unwrap_or(&self.repo)
    }

    /// Does any glob name this package? A Cabal package answers to its
    /// repo's name as well, where the two commonly differ.
    fn named_by(&self, patterns: &[String]) -> bool {
        patterns.iter().any(|p| {
            glob_match(p, &self.name)
                || (self.kind == PackageKind::Cabal && glob_match(p, self.repo_name()))
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Chain {
    pub crates: Vec<CrateNode>,
    /// Every manifest read (roots and members) — where dependents live.
    pub manifests: Vec<(String, PathBuf, PathBuf)>, // (repo, repo_dir, manifest)
    pub warnings: Vec<String>,
}

impl Chain {
    #[must_use]
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.crates.iter().position(|c| c.name == name)
    }
}

/// Read every repo's packages.
///
/// Its cargo crates: the root package, the workspace's members (literal
/// paths and `dir/*` globs, less `exclude`), each member's package; and its
/// root `.cabal` package. A repo with neither contributes nothing.
#[must_use]
pub fn discover(repos: &[(String, PathBuf)]) -> Chain {
    let mut chain = Chain::default();
    for (repo, dir) in repos {
        if let Err(e) = discover_repo(&mut chain, repo, dir) {
            chain.warnings.push(format!("{repo}: {e}"));
        }
        discover_cabal(&mut chain, repo, dir);
    }
    let mut seen: HashMap<String, String> = HashMap::new();
    for c in &chain.crates {
        if let Some(prev) = seen.insert(c.name.clone(), c.repo.clone()) {
            chain.warnings.push(format!(
                "crate {} is in both {prev} and {}; the first is used",
                c.name, c.repo
            ));
        }
    }
    chain
}

/// The `.cabal` package at a repo's root. A test-suite's dependencies count
/// as the library's: `build-depends` is read without its stanzas.
fn discover_cabal(chain: &mut Chain, repo: &str, dir: &Path) {
    use crate::v5::build::cabal;
    let Some(manifest) = cabal::find_cabal_file(dir) else { return };
    let (Some(name), Some(version)) = (cabal::cabal_package_name(dir), cabal::cabal_package_version(dir)) else {
        chain.warnings.push(format!("{repo}: {}: no name or no version", manifest.display()));
        return;
    };
    let deps = cabal::parse_cabal_deps(dir)
        .into_iter()
        .map(|d| DepRef {
            // `pkg:sublib` needs `pkg`.
            package: d.name.split(':').next().unwrap_or(&d.name).to_string(),
            kind: DepKind::Normal,
            req: d.version_req,
        })
        .collect();
    chain.crates.push(CrateNode {
        kind: PackageKind::Cabal,
        name,
        version,
        version_inherited: false,
        dir: dir.to_path_buf(),
        manifest,
        repo: repo.to_string(),
        repo_dir: dir.to_path_buf(),
        ws_root: dir.to_path_buf(),
        publish: true,
        deps,
    });
}

fn read_toml(path: &Path) -> Result<toml::Value, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))
}

fn discover_repo(chain: &mut Chain, repo: &str, dir: &Path) -> Result<(), String> {
    let root_manifest = dir.join("Cargo.toml");
    if !root_manifest.is_file() {
        return Ok(());
    }
    let root = read_toml(&root_manifest)?;
    chain
        .manifests
        .push((repo.to_string(), dir.to_path_buf(), root_manifest.clone()));
    let ws = root.get("workspace").and_then(|w| w.as_table());
    if root.get("package").is_some() {
        chain
            .crates
            .push(package_node(&root, None, repo, dir, dir, &root_manifest)?);
    }
    let Some(ws) = ws else { return Ok(()) };
    let excluded: Vec<PathBuf> = ws
        .get("exclude")
        .and_then(|e| e.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).map(|s| dir.join(s)).collect())
        .unwrap_or_default();
    let patterns: Vec<String> = ws
        .get("members")
        .and_then(|m| m.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let mut members: BTreeSet<PathBuf> = BTreeSet::new();
    for p in &patterns {
        for m in expand_member(dir, p) {
            if m == dir || excluded.iter().any(|x| m.starts_with(x)) {
                continue;
            }
            members.insert(m);
        }
    }
    for m in members {
        let manifest = m.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let v = read_toml(&manifest)?;
        chain
            .manifests
            .push((repo.to_string(), dir.to_path_buf(), manifest.clone()));
        if v.get("package").is_some() {
            chain
                .crates
                .push(package_node(&v, Some(&root), repo, dir, &m, &manifest)?);
        }
    }
    Ok(())
}

/// A member pattern: a literal path, or one whose components carry `*` / `?`.
fn expand_member(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let pattern = pattern.trim_end_matches('/');
    if !pattern.contains(['*', '?']) {
        return vec![root.join(pattern)];
    }
    let mut current = vec![root.to_path_buf()];
    for comp in pattern.split('/') {
        let mut next = Vec::new();
        for base in &current {
            if comp.contains(['*', '?']) {
                let Ok(rd) = std::fs::read_dir(base) else { continue };
                let mut names: Vec<_> = rd
                    .filter_map(Result::ok)
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| glob_match(comp, n))
                    .collect();
                names.sort();
                next.extend(names.into_iter().map(|n| base.join(n)));
            } else {
                next.push(base.join(comp));
            }
        }
        current = next;
    }
    current
}

/// `*` (any run) and `?` (one char) — the include/exclude globs and
/// workspace member patterns.
#[must_use]
pub fn glob_match(pattern: &str, s: &str) -> bool {
    fn go(p: &[char], s: &[char]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], s) || (!s.is_empty() && go(p, &s[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &s[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = s.chars().collect();
    go(&p, &s)
}

fn package_node(
    v: &toml::Value,
    root: Option<&toml::Value>,
    repo: &str,
    repo_dir: &Path,
    dir: &Path,
    manifest: &Path,
) -> Result<CrateNode, String> {
    let pkg = v
        .get("package")
        .and_then(|p| p.as_table())
        .ok_or_else(|| format!("{}: no [package]", manifest.display()))?;
    let name = pkg
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| format!("{}: no package.name", manifest.display()))?
        .to_string();
    let root = root.unwrap_or(v);
    let ws_pkg_version = root
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str());
    let (version, version_inherited) = match pkg.get("version") {
        Some(toml::Value::String(s)) => (s.clone(), false),
        Some(toml::Value::Table(t)) if t.get("workspace").and_then(toml::Value::as_bool) == Some(true) => {
            (ws_pkg_version.unwrap_or("").to_string(), true)
        }
        // No version: cargo reads it as 0.0.0 and refuses to publish it.
        _ => ("0.0.0".to_string(), false),
    };
    let publish = match pkg.get("publish") {
        Some(toml::Value::Boolean(b)) => *b,
        Some(toml::Value::Array(a)) => !a.is_empty(),
        _ => true,
    };
    let ws_deps = root
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(|d| d.as_table());
    let mut deps = Vec::new();
    let mut tables: Vec<(&toml::value::Table, DepKind)> = Vec::new();
    for (key, kind) in [
        ("dependencies", DepKind::Normal),
        ("build-dependencies", DepKind::Build),
        ("dev-dependencies", DepKind::Dev),
    ] {
        if let Some(t) = v.get(key).and_then(|t| t.as_table()) {
            tables.push((t, kind));
        }
        if let Some(targets) = v.get("target").and_then(|t| t.as_table()) {
            for (_, tv) in targets {
                if let Some(t) = tv.get(key).and_then(|t| t.as_table()) {
                    tables.push((t, kind));
                }
            }
        }
    }
    for (table, kind) in tables {
        for (key, val) in table {
            let (package, req) = dep_entry(key, val, ws_deps);
            deps.push(DepRef { package, kind, req });
        }
    }
    Ok(CrateNode {
        kind: PackageKind::Cargo,
        name,
        version,
        version_inherited,
        dir: dir.to_path_buf(),
        manifest: manifest.to_path_buf(),
        repo: repo.to_string(),
        repo_dir: repo_dir.to_path_buf(),
        ws_root: repo_dir.to_path_buf(),
        publish,
        deps,
    })
}

fn dep_entry(
    key: &str,
    val: &toml::Value,
    ws_deps: Option<&toml::value::Table>,
) -> (String, Option<String>) {
    match val {
        toml::Value::String(s) => (key.to_string(), Some(s.clone())),
        toml::Value::Table(t) => {
            if t.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                if let Some(w) = ws_deps.and_then(|w| w.get(key)) {
                    return dep_entry(key, w, None);
                }
            }
            let package = t
                .get("package")
                .and_then(|p| p.as_str())
                .unwrap_or(key)
                .to_string();
            let req = t.get("version").and_then(|v| v.as_str()).map(str::to_string);
            (package, req)
        }
        _ => (key.to_string(), None),
    }
}

// ======================================================================
// Selection and order.
// ======================================================================

/// The crates `include` names (globs on the crate name; a Cabal package's
/// repo name serves too), and their closure.
///
/// With `closure`, every chain crate they need (normal, build, and dev
/// dependencies that carry a version: crates.io holds those too), less
/// `exclude`. Crates with `publish = false` are left out, saying so.
pub fn select(
    chain: &Chain,
    include: &[String],
    exclude: &[String],
    closure: bool,
) -> Result<(Vec<usize>, Vec<String>), String> {
    if include.is_empty() {
        return Err("include is required: name the crates (globs on crate names)".into());
    }
    let mut notes = Vec::new();
    let mut picked: BTreeSet<usize> = BTreeSet::new();
    for (i, c) in chain.crates.iter().enumerate() {
        if c.named_by(include) && !c.named_by(exclude) {
            if chain.index_of(&c.name) != Some(i) {
                continue; // a second crate of that name
            }
            if c.publish {
                picked.insert(i);
                if !include.iter().any(|p| glob_match(p, &c.name)) {
                    notes.push(format!("include {}: the repo of {}", c.repo_name(), c.name));
                }
            } else {
                notes.push(format!("{} is publish = false — left out", c.name));
            }
        }
    }
    if picked.is_empty() {
        return Err(format!("no crate matched include {include:?}"));
    }
    if closure {
        let mut stack: Vec<usize> = picked.iter().copied().collect();
        while let Some(i) = stack.pop() {
            for d in &chain.crates[i].deps {
                if d.kind == DepKind::Dev && d.req.is_none() {
                    continue;
                }
                let Some(j) = chain.index_of(&d.package) else { continue };
                let dep = &chain.crates[j];
                if picked.contains(&j) || dep.named_by(exclude) {
                    continue;
                }
                if !dep.publish {
                    notes.push(format!(
                        "{} needs {}, which is publish = false",
                        chain.crates[i].name, dep.name
                    ));
                    continue;
                }
                notes.push(format!("{} added: {} needs it", dep.name, chain.crates[i].name));
                picked.insert(j);
                stack.push(j);
            }
        }
    }
    Ok((picked.into_iter().collect(), notes))
}

/// The selected crates in dependency order, grouped.
///
/// Consecutive crates of one cargo workspace form a group (one `cargo
/// publish -p … -p …`); a Cabal package is a group of its own.
/// Dev-dependency edges count unless they make a cycle.
pub fn order(chain: &Chain, sel: &[usize]) -> Result<Vec<Vec<usize>>, String> {
    match order_with(chain, sel, true) {
        Ok(o) => Ok(o),
        Err(_) => order_with(chain, sel, false),
    }
}

fn order_with(chain: &Chain, sel: &[usize], dev: bool) -> Result<Vec<Vec<usize>>, String> {
    let in_sel: BTreeSet<usize> = sel.iter().copied().collect();
    let mut needs: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for &i in sel {
        let set = needs.entry(i).or_default();
        for d in &chain.crates[i].deps {
            if d.kind == DepKind::Dev && (!dev || d.req.is_none()) {
                continue;
            }
            if let Some(j) = chain.index_of(&d.package) {
                if j != i && in_sel.contains(&j) {
                    set.insert(j);
                }
            }
        }
    }
    let mut done: BTreeSet<usize> = BTreeSet::new();
    let mut flat: Vec<usize> = Vec::new();
    while done.len() < sel.len() {
        let ready: Vec<usize> = sel
            .iter()
            .copied()
            .filter(|i| !done.contains(i) && needs[i].iter().all(|j| done.contains(j)))
            .collect();
        if ready.is_empty() {
            let stuck: Vec<&str> = sel
                .iter()
                .filter(|i| !done.contains(i))
                .map(|&i| chain.crates[i].name.as_str())
                .collect();
            return Err(format!("dependency cycle among {}", stuck.join(", ")));
        }
        // Keep a cargo workspace together where the order allows.
        let last_ws = flat.last().map(|&i| chain.crates[i].ws_root.clone());
        let pick = ready
            .iter()
            .copied()
            .find(|&i| Some(&chain.crates[i].ws_root) == last_ws.as_ref())
            .unwrap_or(ready[0]);
        done.insert(pick);
        flat.push(pick);
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for i in flat {
        match groups.last_mut() {
            Some(g)
                if chain.crates[i].kind == PackageKind::Cargo
                    && chain.crates[g[0]].kind == PackageKind::Cargo
                    && chain.crates[g[0]].ws_root == chain.crates[i].ws_root =>
            {
                g.push(i);
            }
            _ => groups.push(vec![i]),
        }
    }
    Ok(groups)
}

// ======================================================================
// Versions.
// ======================================================================

/// `patch` / `minor` / `major`, or an exact version.
pub fn next_version(old: &str, kind_or_target: &str) -> Result<String, String> {
    let v = semver::Version::parse(old).map_err(|e| format!("{old}: {e}"))?;
    let new = match kind_or_target {
        "patch" => semver::Version::new(v.major, v.minor, v.patch + 1),
        "minor" => semver::Version::new(v.major, v.minor + 1, 0),
        "major" => semver::Version::new(v.major + 1, 0, 0),
        exact => semver::Version::parse(exact).map_err(|e| format!("{exact}: {e}"))?,
    };
    Ok(new.to_string())
}

/// The requirement a dependent writes when a crate moves `old` → `new`.
///
/// Only a requirement the old version satisfied moves (one on another line
/// — a registry pin on 0.5 — is left alone). In the bumped set a simple
/// requirement (`0.6`, `0.6.0`, `^0.6`, `=0.6.0`, `~0.6.0`) moves to the new
/// version, keeping its operator; elsewhere only a path dependency whose
/// requirement would no longer match moves. `None`: no change.
#[must_use]
pub fn next_requirement(old_req: &str, old: &str, new: &str, in_set: bool, has_path: bool) -> Option<String> {
    let trimmed = old_req.trim();
    let new_v = semver::Version::parse(new).ok()?;
    let old_v = semver::Version::parse(old).ok()?;
    let req = semver::VersionReq::parse(trimmed).ok();
    if !req.as_ref().is_some_and(|r| r.matches(&old_v)) {
        return None;
    }
    if !in_set && (!has_path || req.is_some_and(|r| r.matches(&new_v))) {
        return None;
    }
    let op_len = trimmed
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (op, rest) = trimmed.split_at(op_len);
    let simple = matches!(op.trim(), "" | "^" | "=" | "~")
        && !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_digit() || c == '.');
    let next = if simple { format!("{}{new}", op.trim()) } else { new.to_string() };
    if next == trimmed { None } else { Some(next) }
}

// ======================================================================
// Manifest edits (toml_edit: formatting and comments kept).
// ======================================================================

/// One requirement rewritten in a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReqChange {
    /// The dependency's key in the table (its package may differ).
    pub key: String,
    pub package: String,
    /// `dependencies`, `dev-dependencies`, `target.<cfg>.dependencies`, …
    pub table: String,
    pub old: String,
    pub new: String,
}

/// Set `[package].version`. Returns the old version.
pub fn set_package_version(doc: &mut toml_edit::DocumentMut, new: &str) -> Result<String, String> {
    let pkg = doc
        .get_mut("package")
        .and_then(|p| p.as_table_like_mut())
        .ok_or("no [package]")?;
    let item = pkg.get_mut("version").ok_or("no package.version")?;
    let old = item
        .as_str()
        .ok_or("package.version is not a string (inherited from the workspace?)")?
        .to_string();
    set_str_keep_decor(item, new);
    Ok(old)
}

fn set_str_keep_decor(item: &mut toml_edit::Item, new: &str) {
    let decor = item.as_value().map(|v| v.decor().clone());
    let mut v = toml_edit::Value::from(new);
    if let Some(d) = decor {
        *v.decor_mut() = d;
    }
    *item = toml_edit::Item::Value(v);
}

/// Rewrite every dependency entry on a bumped crate (`bumped`: package →
/// (old, new) version) whose requirement needs it — see [`next_requirement`].
pub fn update_requirements<S: std::hash::BuildHasher>(
    doc: &mut toml_edit::DocumentMut,
    bumped: &HashMap<String, (String, String), S>,
    in_set: bool,
) -> Vec<ReqChange> {
    let mut changes = Vec::new();
    let kinds = ["dependencies", "dev-dependencies", "build-dependencies"];
    let root = doc.as_table_mut();
    for kind in kinds {
        if let Some(t) = root.get_mut(kind).and_then(|t| t.as_table_like_mut()) {
            update_table(t, kind, bumped, in_set, &mut changes);
        }
    }
    if let Some(targets) = root.get_mut("target").and_then(|t| t.as_table_like_mut()) {
        for (cfg, tv) in targets.iter_mut() {
            let Some(tt) = tv.as_table_like_mut() else { continue };
            for kind in kinds {
                if let Some(t) = tt.get_mut(kind).and_then(|t| t.as_table_like_mut()) {
                    let label = format!("target.{cfg}.{kind}");
                    update_table(t, &label, bumped, in_set, &mut changes);
                }
            }
        }
    }
    if let Some(ws) = root.get_mut("workspace").and_then(|t| t.as_table_like_mut()) {
        if let Some(t) = ws.get_mut("dependencies").and_then(|t| t.as_table_like_mut()) {
            update_table(t, "workspace.dependencies", bumped, in_set, &mut changes);
        }
    }
    changes
}

fn update_table<S: std::hash::BuildHasher>(
    table: &mut dyn toml_edit::TableLike,
    label: &str,
    bumped: &HashMap<String, (String, String), S>,
    in_set: bool,
    changes: &mut Vec<ReqChange>,
) {
    for (key, item) in table.iter_mut() {
        let key = key.get().to_string();
        if let Some(s) = item.as_str() {
            let Some((old_v, new)) = bumped.get(&key) else { continue };
            if let Some(next) = next_requirement(s, old_v, new, in_set, false) {
                changes.push(ReqChange {
                    key: key.clone(),
                    package: key.clone(),
                    table: label.to_string(),
                    old: s.to_string(),
                    new: next.clone(),
                });
                set_str_keep_decor(item, &next);
            }
            continue;
        }
        let Some(t) = item.as_table_like_mut() else { continue };
        let package = t
            .get("package")
            .and_then(|p| p.as_str())
            .unwrap_or(&key)
            .to_string();
        let Some((old_v, new)) = bumped.get(&package) else { continue };
        let Some(old) = t.get("version").and_then(|v| v.as_str()).map(str::to_string) else {
            continue; // path-only: nothing to keep resolving
        };
        let has_path = t.contains_key("path");
        if let Some(next) = next_requirement(&old, old_v, new, in_set, has_path) {
            if let Some(v) = t.get_mut("version") {
                set_str_keep_decor(v, &next);
            }
            changes.push(ReqChange {
                key: key.clone(),
                package,
                table: label.to_string(),
                old,
                new: next,
            });
        }
    }
}

// ======================================================================
// The registry (crates.io's sparse index and its downloads).
// ======================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub version: String,
    pub yanked: bool,
}

/// The crates.io sparse index path of a crate name.
#[must_use]
pub fn index_path(name: &str) -> String {
    let n = name.to_ascii_lowercase();
    match n.len() {
        1 => format!("1/{n}"),
        2 => format!("2/{n}"),
        3 => format!("3/{}/{n}", &n[..1]),
        _ => format!("{}/{}/{n}", &n[..2], &n[2..4]),
    }
}

#[derive(Debug, Clone)]
pub struct Registry {
    pub index: String,
    pub dl: String,
}

impl Registry {
    /// crates.io, unless `HYPERFORGE_CRATES_INDEX` / `HYPERFORGE_CRATES_DL`
    /// say otherwise (a `file://` directory is a registry on disk).
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            index: std::env::var("HYPERFORGE_CRATES_INDEX")
                .unwrap_or_else(|_| "https://index.crates.io".into()),
            dl: std::env::var("HYPERFORGE_CRATES_DL")
                .unwrap_or_else(|_| "https://static.crates.io/crates".into()),
        }
    }

    async fn fetch(&self, url: &str) -> Result<Option<Vec<u8>>, String> {
        if let Some(path) = url.strip_prefix("file://") {
            return match std::fs::read(path) {
                Ok(b) => Ok(Some(b)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(format!("{path}: {e}")),
            };
        }
        let client = reqwest::Client::builder()
            .user_agent(concat!("hyperforge/", env!("CARGO_PKG_VERSION"), " (build.publish)"))
            .build()
            .map_err(|e| e.to_string())?;
        let resp = client.get(url).send().await.map_err(|e| format!("{url}: {e}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND
            || resp.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!("{url}: HTTP {}", resp.status()));
        }
        resp.bytes().await.map(|b| Some(b.to_vec())).map_err(|e| e.to_string())
    }

    /// Every version the registry holds of `name` (none: never published).
    pub async fn published(&self, name: &str) -> Result<Vec<Published>, String> {
        let url = format!("{}/{}", self.index.trim_end_matches('/'), index_path(name));
        let Some(body) = self.fetch(&url).await? else { return Ok(Vec::new()) };
        Ok(parse_index(&String::from_utf8_lossy(&body)))
    }

    /// The `.crate` of `name` at `version`.
    pub async fn download(&self, name: &str, version: &str) -> Result<Option<Vec<u8>>, String> {
        let url = format!("{}/{name}/{name}-{version}.crate", self.dl.trim_end_matches('/'));
        self.fetch(&url).await
    }
}

/// One JSON object per line: `{"name", "vers", "yanked", …}`.
#[must_use]
pub fn parse_index(body: &str) -> Vec<Published> {
    body.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|j| {
            Some(Published {
                version: j.get("vers")?.as_str()?.to_string(),
                yanked: j.get("yanked").and_then(serde_json::Value::as_bool).unwrap_or(false),
            })
        })
        .collect()
}

/// Where a local version stands against what the registry holds.
#[must_use]
pub fn registry_status(local: &str, published: &[Published]) -> (&'static str, Option<String>) {
    let latest = published
        .iter()
        .filter(|p| !p.yanked)
        .filter_map(|p| semver::Version::parse(&p.version).ok())
        .max()
        .map(|v| v.to_string());
    if published.is_empty() {
        return ("unpublished", None);
    }
    if let Some(p) = published.iter().find(|p| p.version == local) {
        return (if p.yanked { "yanked" } else { "published" }, latest);
    }
    let behind = match (semver::Version::parse(local), latest.as_deref().map(semver::Version::parse)) {
        (Ok(l), Some(Ok(m))) => l < m,
        _ => false,
    };
    (if behind { "behind" } else { "ahead" }, latest)
}

/// [`registry_status`] for PVP versions, Hackage's `0.5.5.0`, which semver
/// does not read.
#[must_use]
pub fn pvp_status(local: &str, published: &[Published]) -> (&'static str, Option<String>) {
    use std::cmp::Ordering;
    let cmp = |a: &str, b: &str| compare_versions(a, b).unwrap_or(Ordering::Equal);
    let latest = published
        .iter()
        .filter(|p| !p.yanked)
        .map(|p| p.version.as_str())
        .max_by(|a, b| cmp(a, b))
        .map(str::to_string);
    if published.is_empty() {
        return ("unpublished", None);
    }
    if let Some(p) = published.iter().find(|p| p.version == local) {
        return (if p.yanked { "yanked" } else { "published" }, latest);
    }
    let behind = latest.as_deref().is_some_and(|m| cmp(local, m) == Ordering::Less);
    (if behind { "behind" } else { "ahead" }, latest)
}

// ======================================================================
// Drift: has the source changed since the published version?
// ======================================================================

#[derive(Debug, Clone, Default)]
pub struct Drift {
    /// Package files that differ from the published `.crate` (or are gone).
    pub changed: Vec<String>,
    /// The commit `.cargo_vcs_info.json` says it was published from.
    pub published_from: Option<String>,
}

/// Compare a published package against the crate's directory.
///
/// A `.crate`, or a Hackage sdist (the same `<name>-<version>/<rel>`
/// tarball): every file the package carries, `Cargo.toml.orig` against the local Cargo.toml;
/// files git ignores here (a build's output) are not counted.
pub fn drift(crate_bytes: &[u8], dir: &Path) -> Result<Drift, String> {
    let gz = flate2::read::GzDecoder::new(crate_bytes);
    let mut ar = tar::Archive::new(gz);
    let mut out = Drift::default();
    let mut candidates: Vec<String> = Vec::new();
    for entry in ar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        // `<name>-<version>/<rel>`
        let rel: PathBuf = path.components().skip(1).collect();
        let rel_s = rel.to_string_lossy().to_string();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        match rel_s.as_str() {
            "Cargo.toml" | "Cargo.lock" => continue,
            ".cargo_vcs_info.json" => {
                out.published_from = serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .and_then(|j| j.pointer("/git/sha1").and_then(|s| s.as_str()).map(str::to_string));
                continue;
            }
            "Cargo.toml.orig" => {
                if std::fs::read(dir.join("Cargo.toml")).ok().as_deref() != Some(&bytes[..]) {
                    candidates.push("Cargo.toml".into());
                }
                continue;
            }
            _ => {}
        }
        if std::fs::read(dir.join(&rel)).ok().as_deref() != Some(&bytes[..]) {
            candidates.push(rel_s);
        }
    }
    let ignored = git_ignored(dir, &candidates);
    out.changed = candidates.into_iter().filter(|c| !ignored.contains(c)).collect();
    out.changed.sort();
    Ok(out)
}

fn git_ignored(dir: &Path, paths: &[String]) -> BTreeSet<String> {
    use std::io::Write as _;
    if paths.is_empty() {
        return BTreeSet::new();
    }
    let Ok(mut child) = crate::v5::ops::git::git_command()
        .arg("-C")
        .arg(dir)
        .args(["check-ignore", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return BTreeSet::new();
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(paths.join("\n").as_bytes());
    }
    child
        .wait_with_output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).collect())
        .unwrap_or_default()
}

// ======================================================================
// A repo's own publish step (`.hyperforge/config.toml` `[publish]`).
// ======================================================================

/// `[publish]` in a repo's `.hyperforge/config.toml`: a repo's own step.
///
/// For a repo whose crates are not published by plain `cargo publish`
/// (menger builds its app into the package). Each command runs as `sh -c` in the repo with
/// `HF_PUBLISH_CRATES` (the crates hyperforge would publish there),
/// `HF_PUBLISH_MODE` (`dry-run` / `execute`), `HF_CARGO_CONFIG` (a cargo
/// config patching the chain's not-yet-published crates to their checkouts,
/// for a dry run: pass it as `cargo … --config "$HF_CARGO_CONFIG"`) and,
/// on execute, `CARGO_REGISTRY_TOKEN`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PublishStep {
    /// The crates this step publishes (default: every crate of the repo).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crates: Vec<String>,
    /// The dry run: verifies, publishes nothing.
    pub dry_run: String,
    /// The real publish.
    pub execute: String,
}

/// Read `[publish]` from `<repo>/.hyperforge/config.toml`, leniently (the
/// rest of the file is not this reader's business).
pub fn read_publish_step(repo_dir: &Path) -> Result<Option<PublishStep>, String> {
    let path = repo_dir.join(".hyperforge").join("config.toml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let v: toml::Value = toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    match v.get("publish") {
        None => Ok(None),
        Some(p) => p
            .clone()
            .try_into::<PublishStep>()
            .map(Some)
            .map_err(|e| format!("{} [publish]: {e}", path.display())),
    }
}

/// A `[patch.crates-io]` cargo config pointing each `(name, dir)` at its
/// checkout — how a dry run verifies a crate against chain crates that
/// are not on crates.io yet.
#[must_use]
pub fn patch_config(entries: &[(String, PathBuf)]) -> String {
    let mut s = String::from("[patch.crates-io]\n");
    for (name, dir) in entries {
        let path = dir.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
        s.push_str(&format!("\"{name}\" = {{ path = \"{path}\" }}\n"));
    }
    s
}

/// Compare two version strings. Handles both 3-part semver (1.2.3) and
/// 4-part PVP (0.3.0.0) by comparing numeric parts left-to-right,
/// padding the shorter version with zeros.
pub fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let a = a.strip_prefix('v').unwrap_or(a).trim();
    let b = b.strip_prefix('v').unwrap_or(b).trim();

    let a_parts: Vec<u64> = a.split('.').map(|p| p.parse().ok()).collect::<Option<Vec<_>>>()?;
    let b_parts: Vec<u64> = b.split('.').map(|p| p.parse().ok()).collect::<Option<Vec<_>>>()?;

    if a_parts.is_empty() || b_parts.is_empty() {
        return None;
    }

    let max_len = a_parts.len().max(b_parts.len());
    for i in 0..max_len {
        let a_val = a_parts.get(i).copied().unwrap_or(0);
        let b_val = b_parts.get(i).copied().unwrap_or(0);
        match a_val.cmp(&b_val) {
            std::cmp::Ordering::Equal => {}
            other => return Some(other),
        }
    }
    Some(std::cmp::Ordering::Equal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    /// core ← macros-ws{client, macros}; app-ws{api, app} on both.
    fn fixture(root: &Path) -> Vec<(String, PathBuf)> {
        write(
            &root.join("macros/Cargo.toml"),
            "[workspace]\nmembers = [\"client\", \"fixtures/*\"]\nexclude = [\"fixtures/skip\"]\n\n[package]\nname = \"m-macros\"\nversion = \"0.6.0\"\n\n[dev-dependencies]\nm-client = { path = \"client\", version = \"0.6.0\" }\nm-core = { path = \"../core\" }\n",
        );
        write(&root.join("macros/client/Cargo.toml"), "[package]\nname = \"m-client\"\nversion = \"0.6.0\"\n");
        write(&root.join("macros/fixtures/one/Cargo.toml"), "[package]\nname = \"m-fixture\"\nversion = \"0.0.0\"\npublish = false\n");
        write(&root.join("macros/fixtures/skip/Cargo.toml"), "[package]\nname = \"m-skipped\"\nversion = \"0.0.0\"\n");
        write(
            &root.join("core/Cargo.toml"),
            "[package]\nname = \"m-core\"\nversion = \"0.6.0\"\n\n[dependencies]\nm-macros = { path = \"../macros\", version = \"0.6.0\" } # the macros\nm-client = { path = \"../macros/client\", version = \"0.6\" }\nserde = \"1\"\n",
        );
        write(
            &root.join("app/Cargo.toml"),
            "[workspace]\nmembers = [\".\", \"api\"]\n\n[package]\nname = \"m-app\"\nversion = \"0.6.0\"\n\n[dependencies]\nm-api = { path = \"api\", version = \"0.6.0\" }\nm-core = { path = \"../core\", version = \"0.6\" }\n",
        );
        write(
            &root.join("app/api/Cargo.toml"),
            "[package]\nname = \"m-api\"\nversion = \"0.6.0\"\n\n[target.'cfg(unix)'.dependencies]\ncorez = { package = \"m-core\", path = \"../../core\", version = \"0.6.0\" }\n",
        );
        write(
            &root.join("sibling/Cargo.toml"),
            "[package]\nname = \"m-sibling\"\nversion = \"1.0.0\"\n\n[dependencies]\nm-core = { path = \"../core\", version = \"0.6\" }\n",
        );
        ["macros", "core", "app", "sibling"]
            .iter()
            .map(|n| (format!("o/{n}"), root.join(n)))
            .collect()
    }

    fn names(chain: &Chain, idx: &[usize]) -> Vec<String> {
        idx.iter().map(|&i| chain.crates[i].name.clone()).collect()
    }

    #[test]
    fn discovers_members_globs_and_excludes() {
        let t = tempfile::tempdir().unwrap();
        let chain = discover(&fixture(t.path()));
        let mut all: Vec<_> = chain.crates.iter().map(|c| c.name.as_str()).collect();
        all.sort_unstable();
        assert_eq!(all, ["m-api", "m-app", "m-client", "m-core", "m-fixture", "m-macros", "m-sibling"]);
        let api = &chain.crates[chain.index_of("m-api").unwrap()];
        assert_eq!(api.ws_root, t.path().join("app"));
        assert!(api.deps.iter().any(|d| d.package == "m-core" && d.req.as_deref() == Some("0.6.0")));
        assert!(!chain.crates[chain.index_of("m-fixture").unwrap()].publish);
    }

    #[test]
    fn closure_and_order_put_what_a_crate_needs_first() {
        let t = tempfile::tempdir().unwrap();
        let chain = discover(&fixture(t.path()));
        let (sel, notes) = select(&chain, &["m-app".into()], &[], true).unwrap();
        assert_eq!(sel.len(), 5, "{notes:?}");
        let groups = order(&chain, &sel).unwrap();
        let g: Vec<Vec<String>> = groups.iter().map(|g| names(&chain, g)).collect();
        assert_eq!(
            g,
            vec![
                vec!["m-client".to_string(), "m-macros".into()],
                vec!["m-core".into()],
                vec!["m-api".into(), "m-app".into()],
            ]
        );
        // Without the closure, only what include names.
        let (sel, _) = select(&chain, &["m-a*".into()], &["m-api".into()], false).unwrap();
        assert_eq!(names(&chain, &sel), ["m-app"]);
        assert!(select(&chain, &[], &[], true).is_err());
    }

    #[test]
    fn requirements_move_in_the_set_and_only_when_broken_outside() {
        let nr = |req, new, in_set, path| next_requirement(req, "0.6.0", new, in_set, path);
        assert_eq!(nr("0.6.0", "0.6.1", true, true).as_deref(), Some("0.6.1"));
        assert_eq!(nr("0.6", "0.6.1", true, false).as_deref(), Some("0.6.1"));
        assert_eq!(nr("=0.6.0", "0.6.1", true, true).as_deref(), Some("=0.6.1"));
        assert_eq!(nr("0.6", "0.6.1", false, true), None);
        assert_eq!(nr("0.6", "0.7.0", false, true).as_deref(), Some("0.7.0"));
        assert_eq!(nr(">=0.5, <0.7", "0.7.0", false, true).as_deref(), Some("0.7.0"));
        // Another line (a registry pin on 0.5), or not a path dependency: left.
        assert_eq!(nr("0.5", "0.6.1", true, true), None);
        assert_eq!(nr("0.5", "0.6.1", false, true), None);
        assert_eq!(nr("0.6", "0.7.0", false, false), None);
    }

    #[test]
    fn manifest_edits_keep_comments_and_renames() {
        let raw = "[package]\nname = \"m-core\"\nversion = \"0.6.0\" # kept\n\n[dependencies]\nm-macros = { path = \"../macros\", version = \"0.6.0\" } # the macros\ncorez = { package = \"m-client\", path = \"x\", version = \"0.6\" }\nm-path = { path = \"p\" }\n\n[target.'cfg(unix)'.dev-dependencies]\nm-macros = \"0.6.0\"\n";
        let mut doc: toml_edit::DocumentMut = raw.parse().unwrap();
        assert_eq!(set_package_version(&mut doc, "0.6.1").unwrap(), "0.6.0");
        let v = |o: &str, n: &str| (o.to_string(), n.to_string());
        let bumped: HashMap<String, (String, String)> = [
            ("m-macros".to_string(), v("0.6.0", "0.6.1")),
            ("m-client".into(), v("0.6.0", "0.6.1")),
            ("m-path".into(), v("0.6.0", "0.6.1")),
        ]
        .into();
        let ch = update_requirements(&mut doc, &bumped, true);
        let out = doc.to_string();
        assert!(out.contains("version = \"0.6.1\" # kept"), "{out}");
        assert!(out.contains("version = \"0.6.1\" } # the macros"), "{out}");
        assert!(out.contains("corez = { package = \"m-client\", path = \"x\", version = \"0.6.1\" }"), "{out}");
        assert!(out.contains("m-path = { path = \"p\" }"), "{out}");
        assert!(out.contains("[target.'cfg(unix)'.dev-dependencies]\nm-macros = \"0.6.1\""), "{out}");
        assert_eq!(ch.len(), 3);
        assert_eq!(ch[1].key, "corez");
        assert_eq!(ch[1].package, "m-client");
    }

    #[test]
    fn index_paths_and_status() {
        assert_eq!(index_path("a"), "1/a");
        assert_eq!(index_path("ab"), "2/ab");
        assert_eq!(index_path("abc"), "3/a/abc");
        assert_eq!(index_path("Plexus-Core"), "pl/ex/plexus-core");
        let p = parse_index("{\"name\":\"x\",\"vers\":\"0.6.0\",\"yanked\":false}\n{\"name\":\"x\",\"vers\":\"0.6.1\",\"yanked\":true}\n");
        assert_eq!(registry_status("0.6.0", &p), ("published", Some("0.6.0".into())));
        assert_eq!(registry_status("0.6.1", &p).0, "yanked");
        assert_eq!(registry_status("0.6.2", &p).0, "ahead");
        assert_eq!(registry_status("0.5.0", &p).0, "behind");
        assert_eq!(registry_status("0.1.0", &[]).0, "unpublished");
        assert_eq!(next_version("0.6.0", "patch").unwrap(), "0.6.1");
        assert_eq!(next_version("0.6.3", "minor").unwrap(), "0.7.0");
        assert_eq!(next_version("0.6.3", "1.0.0").unwrap(), "1.0.0");
    }

    #[test]
    fn drift_compares_the_published_package_with_the_checkout() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path();
        write(&dir.join("Cargo.toml"), "[package]\nname = \"x\"\nversion = \"0.1.0\"\n");
        write(&dir.join("src/lib.rs"), "pub fn a() {}\n");
        write(&dir.join("src/b.rs"), "pub fn b() {}\n");
        let crate_bytes = make_crate(
            "x-0.1.0",
            &[
                ("Cargo.toml", "normalized"),
                ("Cargo.toml.orig", "[package]\nname = \"x\"\nversion = \"0.1.0\"\n"),
                ("src/lib.rs", "pub fn a() {}\n"),
                ("src/b.rs", "pub fn b() { old }\n"),
                (".cargo_vcs_info.json", "{\"git\":{\"sha1\":\"abc123\"},\"path_in_vcs\":\"\"}"),
            ],
        );
        let d = drift(&crate_bytes, dir).unwrap();
        assert_eq!(d.changed, ["src/b.rs"]);
        assert_eq!(d.published_from.as_deref(), Some("abc123"));
    }

    pub(crate) fn make_crate(prefix: &str, files: &[(&str, &str)]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut b = tar::Builder::new(gz);
        for (name, body) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, format!("{prefix}/{name}"), body.as_bytes()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn a_cabal_package_at_a_repos_root_joins_the_chain() {
        let t = tempfile::tempdir().unwrap();
        let mut repos = fixture(t.path());
        // A repo with both: its Cargo crates and its Cabal package stay apart.
        write(
            &t.path().join("core/m-ffi.cabal"),
            "cabal-version: 3.0\nName: m-ffi\nVersion: 0.5.5.0\n\nlibrary\n  build-depends: base, m-hs:sub >= 1\n",
        );
        write(&t.path().join("hs/m-hs.cabal"), "name: m-hs\nversion: 1.0\n");
        repos.push(("o/hs".into(), t.path().join("hs")));
        let chain = discover(&repos);
        let ffi = &chain.crates[chain.index_of("m-ffi").unwrap()];
        assert_eq!(ffi.kind, PackageKind::Cabal);
        assert_eq!(ffi.version, "0.5.5.0");
        assert!(ffi.deps.iter().any(|d| d.package == "m-hs"));
        // Adjacent, in one directory, they are still two groups.
        let (sel, _) = select(&chain, &["m-core".into(), "m-ffi".into()], &[], false).unwrap();
        let g: Vec<Vec<String>> = order(&chain, &sel).unwrap().iter().map(|g| names(&chain, g)).collect();
        assert_eq!(g, vec![vec!["m-core".to_string()], vec!["m-ffi".into()]]);
        // The closure follows build-depends: m-hs before m-ffi.
        let (sel, _) = select(&chain, &["m-ffi".into()], &[], true).unwrap();
        let g: Vec<Vec<String>> = order(&chain, &sel).unwrap().iter().map(|g| names(&chain, g)).collect();
        assert_eq!(g, vec![vec!["m-hs".to_string()], vec!["m-ffi".into()]]);
    }

    #[test]
    fn pvp_status_reads_four_part_versions() {
        let held = |v: &str, yanked| Published { version: v.into(), yanked };
        let p = [held("0.5.2.0", false), held("0.5.10.0", false), held("0.6.0.0", true)];
        assert_eq!(pvp_status("0.5.2.0", &p), ("published", Some("0.5.10.0".into())));
        assert_eq!(pvp_status("0.6.0.0", &p).0, "yanked");
        assert_eq!(pvp_status("0.5.11.0", &p).0, "ahead");
        assert_eq!(pvp_status("0.5.5.0", &p).0, "behind");
        assert_eq!(pvp_status("3.15", &[]).0, "unpublished");
    }

    #[test]
    fn publish_step_reads_leniently() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(read_publish_step(t.path()).unwrap(), None);
        write(
            &t.path().join(".hyperforge/config.toml"),
            "repo_name = \"x\"\norg = \"o\"\n\n[publish]\ncrates = [\"a\"]\ndry_run = \"scripts/publish.sh dry-run\"\nexecute = \"scripts/publish.sh publish\"\n",
        );
        let s = read_publish_step(t.path()).unwrap().unwrap();
        assert_eq!(s.crates, ["a"]);
        assert_eq!(s.execute, "scripts/publish.sh publish");
        assert!(patch_config(&[("a".into(), PathBuf::from("/x/a"))]).contains("\"a\" = { path = \"/x/a\" }"));
    }
}

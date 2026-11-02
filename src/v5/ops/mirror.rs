//! HYPERFORGE-60: push a checkout's branches and tags to every forge its
//! repo is registered on. Targets are the registry's remotes (the org
//! yaml), not `.git/config`, so a mirror needs no extra git remote. Pushes
//! are never forced; a token, when the forge has one in the secret store,
//! reaches `git` through `GIT_ASKPASS` and the environment, never through
//! arguments, URLs or logs.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::v5::config::{CredentialType, DomainName, OrgConfig, OrgRepo, ProviderKind};
use crate::v5::ops::repo::{derive_provider, filter_remotes_by_forges};
use crate::v5::secrets::{SecretRef, SecretResolver};

/// One forge a repo is pushed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub url: String,
    pub provider: ProviderKind,
}

#[must_use]
pub const fn provider_label(p: ProviderKind) -> &'static str {
    match p {
        ProviderKind::Github => "github",
        ProviderKind::Codeberg => "codeberg",
        ProviderKind::Gitlab => "gitlab",
    }
}

/// Every registered remote of `repo` in its forge scope, with its provider.
/// A remote whose provider can't be told is returned as an error so the
/// caller can say which, instead of silently dropping it.
pub fn targets(
    repo: &OrgRepo,
    provider_map: &BTreeMap<DomainName, ProviderKind>,
) -> (Vec<Target>, Vec<String>) {
    let mut out = Vec::new();
    let mut unknown = Vec::new();
    for r in filter_remotes_by_forges(repo, provider_map) {
        match derive_provider(r, provider_map) {
            Ok(provider) => out.push(Target { url: r.url.as_str().to_string(), provider }),
            Err(e) => unknown.push(e),
        }
    }
    // Remotes the scope filter dropped because their provider is unknown
    // are reported too (the filter skips them without a word).
    for r in &repo.remotes {
        if derive_provider(r, provider_map).is_err() && !out.iter().any(|t| t.url == r.url.as_str()) {
            let msg = format!("cannot tell the forge of '{}'", r.url.as_str());
            if !unknown.contains(&msg) {
                unknown.push(msg);
            }
        }
    }
    (out, unknown)
}

/// The secret-store references tried for a forge, in order: the org's own
/// credential for that provider (its primary `forge` block, or the
/// `mirrors` block naming the provider), then `<provider>/<owner>/token`,
/// then `<provider>/_default/token`.
#[must_use]
pub fn credential_refs(org: &OrgConfig, provider: ProviderKind, owner: Option<&str>) -> Vec<String> {
    let mut refs = Vec::new();
    let blocks = std::iter::once(&org.forge).chain(org.mirrors.iter());
    for b in blocks.filter(|b| b.provider == provider) {
        for c in b.credentials.iter().filter(|c| matches!(c.cred_type, CredentialType::Token)) {
            refs.push(c.key.clone());
        }
    }
    let p = provider_label(provider);
    if let Some(o) = owner.filter(|o| !o.is_empty()) {
        refs.push(format!("secrets://{p}/{o}/token"));
    }
    refs.push(format!("secrets://{p}/_default/token"));
    refs.dedup();
    refs
}

/// Is this a URL `git` authenticates over HTTP (where a token applies)?
#[must_use]
pub fn is_http(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// Owner segment of a forge URL (`https://host/<owner>/<repo>.git`,
/// `git@host:<owner>/<repo>.git`).
#[must_use]
pub fn url_owner(url: &str) -> Option<String> {
    crate::v5::adapters::parse_remote_url(url).map(|(_, o, _)| o)
}

/// A resolved token and the reference it came from (the reference may be
/// shown; the token never is).
pub struct Credential {
    pub reference: String,
    token: String,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential").field("reference", &self.reference).finish_non_exhaustive()
    }
}

/// First reference in `refs` the store can resolve.
pub fn resolve(resolver: &dyn SecretResolver, refs: &[String]) -> Option<Credential> {
    refs.iter().find_map(|r| {
        let token = resolver.resolve(&SecretRef::parse(r).ok()?).ok()?;
        (!token.is_empty()).then(|| Credential { reference: r.clone(), token })
    })
}

/// The refspecs a mirror pushes: named branches (or every local branch),
/// plus every tag. No `+`: a forge that has moved on rejects the ref.
pub fn refspecs(dir: &Path, branches: &[String], all_branches: bool, tags: bool) -> Result<Vec<String>, String> {
    let mut specs = Vec::new();
    if all_branches {
        specs.push("refs/heads/*:refs/heads/*".to_string());
    } else {
        for b in branches {
            let b = b.trim();
            if b.is_empty() || b.starts_with('+') || b.contains(':') {
                return Err(format!("'{b}' is not a branch name"));
            }
            let ok = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{b}")])
                .output()
                .map_err(|e| e.to_string())?
                .status
                .success();
            if !ok {
                return Err(format!("no local branch '{b}' in {}", dir.display()));
            }
            specs.push(format!("refs/heads/{b}:refs/heads/{b}"));
        }
    }
    if tags {
        specs.push("refs/tags/*:refs/tags/*".to_string());
    }
    if specs.is_empty() {
        return Err("nothing to push: no branch and tags off".into());
    }
    Ok(specs)
}

/// The username a forge expects beside a token over HTTP.
fn http_user(provider: ProviderKind, url: &str) -> String {
    match provider {
        ProviderKind::Github => "x-access-token".into(),
        ProviderKind::Gitlab => "oauth2".into(),
        // Gitea/Forgejo take any username when the password is a token.
        ProviderKind::Codeberg => url_owner(url).unwrap_or_else(|| "git".into()),
    }
}

const ASKPASS: &str = "#!/bin/sh\ncase \"$1\" in\n  Username*) printf '%s\\n' \"$HF_MIRROR_USER\" ;;\n  *) printf '%s\\n' \"$HF_MIRROR_TOKEN\" ;;\nesac\n";

/// A one-use askpass helper. It holds no secret: it echoes the
/// environment it is run with.
struct Askpass {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn askpass() -> Result<Askpass, String> {
    let dir = tempfile::Builder::new().prefix("hf-mirror-").tempdir().map_err(|e| e.to_string())?;
    let path = dir.path().join("askpass.sh");
    let mut f = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    f.write_all(ASKPASS.as_bytes()).map_err(|e| e.to_string())?;
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    Ok(Askpass { _dir: dir, path })
}

/// The `git` command for one push or listing. Pure, so tests can check that
/// the token is in the environment only.
fn git_command(dir: &Path, provider: ProviderKind, url: &str, cred: Option<&Credential>, askpass: Option<&Path>, tail: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if let (Some(c), Some(a), true) = (cred, askpass, is_http(url)) {
        // Only this token: no stored helper answers instead.
        cmd.args(["-c", "credential.helper="]);
        cmd.env("GIT_ASKPASS", a);
        cmd.env("HF_MIRROR_USER", http_user(provider, url));
        cmd.env("HF_MIRROR_TOKEN", &c.token);
    }
    cmd.args(tail);
    cmd
}

/// What one push did, ref by ref (`git push --porcelain`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pushed {
    /// Refs created or moved forward.
    pub updated: Vec<String>,
    /// Refs already the same.
    pub up_to_date: u32,
    /// Refs the forge refused, with git's reason.
    pub rejected: Vec<String>,
}

fn parse_porcelain(out: &str) -> Pushed {
    let mut p = Pushed::default();
    for line in out.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(flag), Some(spec)) = (parts.next(), parts.next()) else { continue };
        let summary = parts.next().unwrap_or("").trim();
        let to = spec.split(':').nth(1).unwrap_or(spec).to_string();
        match flag.trim_end() {
            "=" => p.up_to_date += 1,
            "!" => p.rejected.push(format!("{to} ({summary})")),
            "" | "*" | "+" | "-" => p.updated.push(to),
            _ => {}
        }
    }
    p
}

/// Push `specs` to `url`. `Ok` carries the per-ref outcome even when some
/// refs were rejected; `Err` means git could not push at all.
pub fn push(dir: &Path, target: &Target, specs: &[String], cred: Option<&Credential>) -> Result<Pushed, String> {
    let helper = match cred {
        Some(_) if is_http(&target.url) => Some(askpass()?),
        _ => None,
    };
    let mut tail: Vec<&str> = vec!["push", "--porcelain", &target.url];
    tail.extend(specs.iter().map(String::as_str));
    let out = git_command(dir, target.provider, &target.url, cred, helper.as_ref().map(|h| h.path.as_path()), &tail)
        .output()
        .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let pushed = parse_porcelain(&stdout);
    if out.status.success() || !pushed.rejected.is_empty() || !pushed.updated.is_empty() || pushed.up_to_date > 0 {
        return Ok(pushed);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(redact(stderr.trim(), cred))
}

/// What a forge holds compared with the checkout.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Compared {
    /// `(branch, state)`: `same`, `missing` (forge lacks it), `differs`.
    pub branches: Vec<(String, String)>,
    pub tags_missing: u32,
    pub tags_differ: u32,
}

fn local_refs(dir: &Path, pattern: &str) -> BTreeMap<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["for-each-ref", "--format=%(objectname) %(refname)", pattern])
        .output();
    let mut m = BTreeMap::new();
    if let Ok(o) = out {
        for l in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some((sha, name)) = l.split_once(' ') {
                m.insert(name.to_string(), sha.to_string());
            }
        }
    }
    m
}

/// Compare the checkout's branches (named, or all) and tags with what the
/// forge advertises. Reads only (`git ls-remote`).
pub fn compare(dir: &Path, target: &Target, branches: &[String], all_branches: bool, cred: Option<&Credential>) -> Result<Compared, String> {
    let helper = match cred {
        Some(_) if is_http(&target.url) => Some(askpass()?),
        _ => None,
    };
    let out = git_command(dir, target.provider, &target.url, cred, helper.as_ref().map(|h| h.path.as_path()), &["ls-remote", "--heads", "--tags", &target.url])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(redact(String::from_utf8_lossy(&out.stderr).trim(), cred));
    }
    let mut remote = BTreeMap::new();
    for l in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((sha, name)) = l.split_once('\t') {
            if !name.ends_with("^{}") {
                remote.insert(name.to_string(), sha.to_string());
            }
        }
    }
    let heads = local_refs(dir, "refs/heads");
    let mut c = Compared::default();
    let wanted: Vec<String> = if all_branches {
        heads.keys().filter_map(|k| k.strip_prefix("refs/heads/")).map(String::from).collect()
    } else {
        branches.to_vec()
    };
    for b in wanted {
        let name = format!("refs/heads/{b}");
        let state = match (heads.get(&name), remote.get(&name)) {
            (Some(l), Some(r)) if l == r => "same",
            (_, None) => "missing",
            _ => "differs",
        };
        c.branches.push((b, state.to_string()));
    }
    for (name, sha) in local_refs(dir, "refs/tags") {
        match remote.get(&name) {
            None => c.tags_missing += 1,
            Some(r) if r != &sha => c.tags_differ += 1,
            Some(_) => {}
        }
    }
    Ok(c)
}

/// Strip a token from text git printed, in case a forge echoed it.
fn redact(s: &str, cred: Option<&Credential>) -> String {
    match cred {
        Some(c) if !c.token.is_empty() => s.replace(&c.token, "<redacted>"),
        _ => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v5::config::{CredentialEntry, ForgeBlock, OrgName};

    fn org(mirrors: Vec<ForgeBlock>) -> OrgConfig {
        OrgConfig {
            name: OrgName::from("acme"),
            forge: ForgeBlock {
                provider: ProviderKind::Github,
                credentials: vec![CredentialEntry { key: "secrets://github/acme/token".into(), cred_type: CredentialType::Token }],
            },
            mirrors,
            repos: Vec::new(),
        }
    }

    #[test]
    fn each_forge_gets_its_own_credential() {
        let o = org(vec![ForgeBlock {
            provider: ProviderKind::Codeberg,
            credentials: vec![CredentialEntry { key: "secrets://codeberg/acme-cb/token".into(), cred_type: CredentialType::Token }],
        }]);
        assert_eq!(credential_refs(&o, ProviderKind::Github, Some("acme"))[0], "secrets://github/acme/token");
        let cb = credential_refs(&o, ProviderKind::Codeberg, Some("acme"));
        assert_eq!(cb, vec!["secrets://codeberg/acme-cb/token", "secrets://codeberg/acme/token", "secrets://codeberg/_default/token"]);
        let gl = credential_refs(&org(Vec::new()), ProviderKind::Gitlab, None);
        assert_eq!(gl, vec!["secrets://gitlab/_default/token"]);
    }

    #[test]
    fn the_token_travels_in_the_environment_only() {
        let cred = Credential { reference: "secrets://github/acme/token".into(), token: "tok-SECRET".into() };
        let target = Target { url: "https://github.com/acme/widget.git".into(), provider: ProviderKind::Github };
        let cmd = git_command(Path::new("/x"), target.provider, &target.url, Some(&cred), Some(Path::new("/a/askpass.sh")), &["push", "--porcelain", &target.url, "refs/tags/*:refs/tags/*"]);
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.iter().all(|a| !a.contains("tok-SECRET")), "{args:?}");
        assert!(args.contains(&"credential.helper=".to_string()));
        let envs: BTreeMap<String, String> = cmd
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_string_lossy().into_owned(), v?.to_string_lossy().into_owned())))
            .collect();
        assert_eq!(envs.get("HF_MIRROR_TOKEN").map(String::as_str), Some("tok-SECRET"));
        assert_eq!(envs.get("HF_MIRROR_USER").map(String::as_str), Some("x-access-token"));
        assert_eq!(format!("{cred:?}").contains("tok-SECRET"), false);
        // SSH and local remotes never get the token.
        let ssh = git_command(Path::new("/x"), ProviderKind::Codeberg, "git@codeberg.org:acme/w.git", Some(&cred), Some(Path::new("/a")), &["push"]);
        assert!(ssh.get_envs().all(|(k, _)| k != "HF_MIRROR_TOKEN"));
    }

    #[test]
    fn the_askpass_helper_answers_from_its_environment() {
        let a = askpass().unwrap();
        let run = |prompt: &str| {
            let o = Command::new(&a.path).arg(prompt).env("HF_MIRROR_USER", "u").env("HF_MIRROR_TOKEN", "t").output().unwrap();
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        assert_eq!(run("Username for 'https://codeberg.org': "), "u");
        assert_eq!(run("Password for 'https://u@codeberg.org': "), "t");
        // The helper on disk is the fixed script: no secret is ever written.
        assert_eq!(std::fs::read_to_string(&a.path).unwrap(), ASKPASS);
    }

    #[test]
    fn porcelain_lines_sort_into_updated_same_and_rejected() {
        let out = "To /tmp/b\n*\trefs/heads/feat:refs/heads/feat\t[new branch]\n \trefs/heads/main:refs/heads/main\ta..b\n=\trefs/tags/v1:refs/tags/v1\t[up to date]\n!\trefs/tags/v2:refs/tags/v2\t[rejected] (already exists)\nDone\n";
        let p = parse_porcelain(out);
        assert_eq!(p.updated, vec!["refs/heads/feat", "refs/heads/main"]);
        assert_eq!(p.up_to_date, 1);
        assert_eq!(p.rejected, vec!["refs/tags/v2 ([rejected] (already exists))"]);
    }

    #[test]
    fn a_branch_name_cannot_smuggle_a_force_or_a_rename() {
        let d = tempfile::tempdir().unwrap();
        assert!(refspecs(d.path(), &["+main".into()], false, false).is_err());
        assert!(refspecs(d.path(), &["main:other".into()], false, false).is_err());
        assert_eq!(refspecs(d.path(), &[], true, true).unwrap(), vec!["refs/heads/*:refs/heads/*", "refs/tags/*:refs/tags/*"]);
        assert!(refspecs(d.path(), &[], false, false).is_err());
    }
}

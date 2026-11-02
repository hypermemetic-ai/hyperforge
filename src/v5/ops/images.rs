//! Container images on a forge's registry: GitHub (ghcr.io) and Codeberg.
//!
//! Listing and deleting go through the forge's packages API; pushing goes
//! through the container CLI (`docker`, or `HF_CONTAINER_CLI`). Everything
//! that can be refused is refused before anything is written. Tokens stay in
//! request headers and, for a push, in a throwaway client config fed on
//! stdin; never in arguments, events or errors.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use reqwest::{header, Client, Method, RequestBuilder, Response, StatusCode};
use serde_json::Value;
use tokio::io::AsyncWriteExt as _;

use crate::v5::config::ProviderKind;
use crate::v5::secrets::{SecretRef, SecretResolver as _, YamlSecretStore};

pub type Refused = (&'static str, String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Registry {
    Ghcr,
    Codeberg,
}

impl Registry {
    pub const fn host(self) -> &'static str {
        match self {
            Self::Ghcr => "ghcr.io",
            Self::Codeberg => "codeberg.org",
        }
    }

    pub const fn forge(self) -> &'static str {
        match self {
            Self::Ghcr => "github",
            Self::Codeberg => "codeberg",
        }
    }

    pub fn parse(s: &str) -> Result<Self, Refused> {
        match s {
            "github" | "ghcr" | "ghcr.io" => Ok(Self::Ghcr),
            "codeberg" | "codeberg.org" => Ok(Self::Codeberg),
            other => Err(("unsupported", format!("{other}: images are on github (ghcr.io) and codeberg only"))),
        }
    }

    pub fn from_provider(p: ProviderKind) -> Result<Self, Refused> {
        match p {
            ProviderKind::Github => Ok(Self::Ghcr),
            ProviderKind::Codeberg => Ok(Self::Codeberg),
            ProviderKind::Gitlab => Err(("unsupported", "gitlab: images are on github (ghcr.io) and codeberg only".into())),
        }
    }
}

/// The registry for `org`: `forge` when given, else the org's own forge.
pub fn registry_for(config_dir: &Path, org: &str, forge: Option<&str>) -> Result<Registry, Refused> {
    if let Some(f) = forge {
        return Registry::parse(f);
    }
    let orgs = crate::v5::ops::state::load_orgs(&config_dir.join("orgs"))
        .map_err(|e| ("io", e.to_string()))?;
    let found = orgs.iter().find(|(name, _)| name.as_str() == org);
    let Some((_, cfg)) = found else {
        return Err(("not_found", format!("no org {org}: pass --forge github|codeberg")));
    };
    Registry::from_provider(cfg.forge.provider)
}

pub struct Package {
    pub name: String,
    pub created_at: Option<String>,
    pub visibility: Option<String>,
}

#[derive(Clone)]
pub struct Version {
    pub id: u64,
    /// `sha256:…`, when the registry says.
    pub digest: Option<String>,
    pub tags: Vec<String>,
    pub created_at: Option<String>,
}

/// A registry's packages API, signed in as the org.
pub struct Api {
    pub registry: Registry,
    pub owner: String,
    http: Client,
    base: String,
    token: Option<String>,
}

fn net(e: reqwest::Error) -> Refused {
    ("network", e.without_url().to_string())
}

async fn refused(what: &str, r: Response) -> Refused {
    let status = r.status();
    let body: Value = r.json().await.unwrap_or_default();
    let detail = body["message"].as_str().unwrap_or("");
    let code = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "auth",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY => "conflict",
        _ => "forge",
    };
    let hint = if code == "auth" { " (the token needs read:packages, delete:packages or write:packages)" } else { "" };
    (code, format!("{what}: {status} {detail}{hint}"))
}

/// The token for `owner` on `registry`: a packages token first, then the
/// org's forge token, then the forge's default.
pub fn token(config_dir: &Path, registry: Registry, owner: &str) -> Option<String> {
    let store = YamlSecretStore::new(config_dir);
    let f = registry.forge();
    [
        format!("secrets://{f}/{owner}/packages_token"),
        format!("secrets://{f}/{owner}/token"),
        format!("secrets://{f}/_default/packages_token"),
        format!("secrets://{f}/_default/token"),
    ]
    .iter()
    .find_map(|r| store.resolve(&SecretRef::parse(r).ok()?).ok())
}

/// Tag names a registry takes (the OCI distribution spec's rule).
pub fn valid_tag(t: &str) -> bool {
    let mut chars = t.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric() || c == '_')
        && t.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Image names a registry takes: lowercase path components.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.split('/').all(|part| {
            !part.is_empty()
                && part.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
                && part.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        })
}

impl Api {
    /// `endpoint` replaces the forge's API root (tests).
    pub fn new(config_dir: &Path, registry: Registry, owner: &str, endpoint: Option<String>) -> Self {
        let base = endpoint.unwrap_or_else(|| match registry {
            Registry::Ghcr => "https://api.github.com".into(),
            Registry::Codeberg => "https://codeberg.org/api/v1".into(),
        });
        Self {
            registry,
            owner: owner.to_string(),
            http: Client::new(),
            base,
            token: token(config_dir, registry, owner),
        }
    }

    fn request(&self, method: Method, url: String) -> Result<RequestBuilder, Refused> {
        let token = self.token.as_ref().ok_or_else(|| {
            let f = self.registry.forge();
            ("auth", format!("no token at secrets://{f}/{}/packages_token, {f}/{}/token or {f}/_default", self.owner, self.owner))
        })?;
        let auth = match self.registry {
            Registry::Ghcr => format!("Bearer {token}"),
            Registry::Codeberg => format!("token {token}"),
        };
        let mut r = self.http.request(method, url).header(header::AUTHORIZATION, auth).header(header::USER_AGENT, "hyperforge");
        if self.registry == Registry::Ghcr {
            r = r.header(header::ACCEPT, "application/vnd.github+json");
        }
        Ok(r)
    }

    /// `Some` with the answer, `None` on a 404.
    async fn get(&self, what: &str, url: String) -> Result<Option<Value>, Refused> {
        let r = self.request(Method::GET, url)?.send().await.map_err(net)?;
        match r.status() {
            StatusCode::NOT_FOUND => Ok(None),
            s if s.is_success() => Ok(Some(r.json().await.map_err(net)?)),
            _ => Err(refused(what, r).await),
        }
    }

    /// GitHub keeps an org's packages under `/orgs/`, a person's under `/users/`.
    async fn github_root(&self, what: &str, rest: &str) -> Result<Option<String>, Refused> {
        for kind in ["orgs", "users"] {
            let root = format!("{}/{kind}/{}", self.base, self.owner);
            if self.get(what, format!("{root}{rest}")).await?.is_some() {
                return Ok(Some(root));
            }
        }
        Ok(None)
    }

    /// Every page of a list endpoint; `None` when the first page is a 404.
    async fn pages(&self, what: &str, url: &str, per: usize) -> Result<Option<Vec<Value>>, Refused> {
        let mut all = Vec::new();
        for page in 1.. {
            let sep = if url.contains('?') { '&' } else { '?' };
            let full = format!("{url}{sep}per_page={per}&limit={per}&page={page}");
            let Some(v) = self.get(what, full).await? else {
                return Ok(if page == 1 { None } else { Some(all) });
            };
            let items = v.as_array().cloned().unwrap_or_default();
            let n = items.len();
            all.extend(items);
            if n < per {
                break;
            }
        }
        Ok(Some(all))
    }

    /// Every container package the owner has.
    pub async fn packages(&self) -> Result<Vec<Package>, Refused> {
        match self.registry {
            Registry::Ghcr => {
                let q = "/packages?package_type=container";
                let Some(root) = self.github_root("list packages", q).await? else {
                    return Err(("not_found", format!("{} is not a GitHub org or user", self.owner)));
                };
                let items = self.pages("list packages", &format!("{root}{q}"), 100).await?.unwrap_or_default();
                Ok(items
                    .iter()
                    .filter_map(|p| {
                        Some(Package {
                            name: p["name"].as_str()?.to_string(),
                            created_at: p["created_at"].as_str().map(String::from),
                            visibility: p["visibility"].as_str().map(String::from),
                        })
                    })
                    .collect())
            }
            Registry::Codeberg => {
                let url = format!("{}/packages/{}?type=container", self.base, self.owner);
                let items = self.pages("list packages", &url, 50).await?.unwrap_or_default();
                let mut out: Vec<Package> = Vec::new();
                for p in &items {
                    let Some(name) = p["name"].as_str() else { continue };
                    if out.iter().any(|o| o.name == name) {
                        continue;
                    }
                    out.push(Package {
                        name: name.to_string(),
                        created_at: p["created_at"].as_str().map(String::from),
                        visibility: None,
                    });
                }
                Ok(out)
            }
        }
    }

    /// Every version of one image, with its tags; empty when there is no
    /// such image yet.
    pub async fn versions(&self, name: &str) -> Result<Vec<Version>, Refused> {
        let enc = urlencoding::encode(name).into_owned();
        match self.registry {
            Registry::Ghcr => {
                let rest = format!("/packages/container/{enc}/versions");
                let Some(root) = self.github_root("list versions", &rest).await? else {
                    return Ok(Vec::new());
                };
                let items = self.pages("list versions", &format!("{root}{rest}"), 100).await?.unwrap_or_default();
                Ok(items
                    .iter()
                    .filter_map(|v| {
                        Some(Version {
                            id: v["id"].as_u64()?,
                            digest: v["name"].as_str().map(String::from),
                            tags: v["metadata"]["container"]["tags"]
                                .as_array()
                                .map(|t| t.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                                .unwrap_or_default(),
                            created_at: v["created_at"].as_str().map(String::from),
                        })
                    })
                    .collect())
            }
            Registry::Codeberg => {
                let url = format!("{}/packages/{}?type=container&q={enc}", self.base, self.owner);
                let items = self.pages("list versions", &url, 50).await?.unwrap_or_default();
                Ok(items
                    .iter()
                    .filter(|p| p["name"].as_str() == Some(name))
                    .filter_map(|p| {
                        let version = p["version"].as_str()?.to_string();
                        let is_digest = version.starts_with("sha256:");
                        Some(Version {
                            id: p["id"].as_u64().unwrap_or(0),
                            digest: is_digest.then(|| version.clone()),
                            tags: if is_digest { Vec::new() } else { vec![version] },
                            created_at: p["created_at"].as_str().map(String::from),
                        })
                    })
                    .collect())
            }
        }
    }

    /// The account the token signs in as (a registry login's user name).
    pub async fn login_name(&self) -> Result<String, Refused> {
        let v = self.get("read the token's user", format!("{}/user", self.base)).await?;
        v.and_then(|v| v["login"].as_str().map(String::from))
            .ok_or_else(|| ("auth", "the token names no user".to_string()))
    }

    /// Delete one version (on GitHub every tag it carries goes with it).
    pub async fn delete(&self, name: &str, version: &Version, by: &str) -> Result<(), Refused> {
        let enc = urlencoding::encode(name).into_owned();
        let url = match self.registry {
            Registry::Ghcr => {
                let rest = format!("/packages/container/{enc}/versions");
                let root = self
                    .github_root("find the image", &rest)
                    .await?
                    .ok_or_else(|| ("not_found", format!("no image {name}")))?;
                format!("{root}{rest}/{}", version.id)
            }
            Registry::Codeberg => format!("{}/packages/{}/container/{enc}/{}", self.base, self.owner, urlencoding::encode(by)),
        };
        let r = self.request(Method::DELETE, url)?.send().await.map_err(net)?;
        if r.status().is_success() {
            Ok(())
        } else {
            Err(refused(&format!("delete {name}:{by}"), r).await)
        }
    }
}

/// What a delete would remove: the version a tag (or `sha256:` digest)
/// names, and the other tags that would go with it.
pub struct DeletePlan {
    pub version: Version,
    pub by: String,
    pub other_tags: Vec<String>,
}

pub fn plan_delete(versions: &[Version], name: &str, by: &str, with_other_tags: bool) -> Result<DeletePlan, Refused> {
    let found = versions.iter().find(|v| {
        if by.starts_with("sha256:") {
            v.digest.as_deref() == Some(by)
        } else {
            v.tags.iter().any(|t| t == by)
        }
    });
    let version = found.cloned().ok_or_else(|| ("not_found", format!("{name} has no tag or digest {by}")))?;
    let other_tags: Vec<String> = version.tags.iter().filter(|t| *t != by).cloned().collect();
    if !other_tags.is_empty() && !with_other_tags {
        return Err((
            "conflict",
            format!("{name}:{by} is the same image as {}: deleting it deletes those too; pass --with-other-tags true", other_tags.join(", ")),
        ));
    }
    Ok(DeletePlan { version, by: by.to_string(), other_tags })
}

/// A push, checked: where the image comes from and every reference it goes to.
pub struct PushPlan {
    pub registry: Registry,
    pub owner: String,
    pub targets: Vec<String>,
    pub source: Source,
    cli: String,
}

pub enum Source {
    /// Build from a context directory and its Dockerfile.
    Build { context: PathBuf, dockerfile: PathBuf },
    /// An image already built here.
    Local(String),
}

impl Source {
    pub fn describe(&self) -> String {
        match self {
            Self::Build { context, dockerfile } => format!("build {} ({})", context.display(), dockerfile.display()),
            Self::Local(image) => format!("local image {image}"),
        }
    }
}

pub struct PushArgs {
    pub owner: String,
    pub registry: Registry,
    pub name: String,
    pub tags: Vec<String>,
    pub context: Option<PathBuf>,
    pub dockerfile: Option<String>,
    pub image: Option<String>,
    /// The container CLI (default: `HF_CONTAINER_CLI`, else `docker`).
    pub cli: Option<String>,
}

fn cli(given: Option<String>) -> String {
    given.or_else(|| std::env::var("HF_CONTAINER_CLI").ok()).unwrap_or_else(|| "docker".into())
}

async fn run(cli: &str, args: &[&str], config: Option<&Path>, stdin: Option<&str>) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(cli);
    if let Some(c) = config {
        cmd.arg("--config").arg(c);
    }
    cmd.args(args).stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("{cli}: {e}"))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes()).await.map_err(|e| e.to_string())?;
        drop(pipe);
    }
    let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<&str> = err.lines().collect();
        Err(lines[lines.len().saturating_sub(5)..].join(" | "))
    }
}

/// Check a push; runs nothing that writes.
pub async fn plan_push(a: PushArgs) -> Result<PushPlan, Refused> {
    if !valid_name(&a.name) {
        return Err(("validation", format!("{}: an image name is lowercase letters, digits, '.', '_', '-'", a.name)));
    }
    if a.tags.is_empty() {
        return Err(("validation", "an image push needs a tag".into()));
    }
    if let Some(bad) = a.tags.iter().find(|t| !valid_tag(t)) {
        return Err(("validation", format!("{bad}: not a valid image tag")));
    }
    let cli = cli(a.cli);
    let source = match (a.context, a.image) {
        (Some(_), Some(_)) => return Err(("validation", "pass --path (build) or --image (push one built here), not both".into())),
        (None, None) => return Err(("validation", "pass --path (build) or --image (push one built here)".into())),
        (Some(ctx), None) => {
            if !ctx.is_dir() {
                return Err(("not_found", format!("{}: no such directory", ctx.display())));
            }
            let candidates: Vec<String> = match a.dockerfile {
                Some(d) => vec![d],
                None => ["Dockerfile", "Containerfile", "docker/Dockerfile"].map(String::from).to_vec(),
            };
            let Some(df) = candidates.iter().map(|c| ctx.join(c)).find(|p| p.is_file()) else {
                return Err(("not_found", format!("no {} in {}", candidates.join(" or "), ctx.display())));
            };
            Source::Build { context: ctx, dockerfile: df }
        }
        (None, Some(image)) => {
            run(&cli, &["image", "inspect", "--format", "{{.Id}}", &image], None, None)
                .await
                .map_err(|e| ("not_found", format!("no local image {image}: {e}")))?;
            Source::Local(image)
        }
    };
    let host = a.registry.host();
    let targets = a.tags.iter().map(|t| format!("{host}/{}/{}:{t}", a.owner, a.name)).collect();
    Ok(PushPlan { registry: a.registry, owner: a.owner, targets, source, cli })
}

impl PushPlan {
    /// Build (or tag) and push every target, signed in with `token` through
    /// a throwaway client config; with no token, the CLI's own stored login.
    /// Stops at the first failure, naming what was pushed and what was not.
    pub async fn execute(&self, login: Option<(String, String)>, mut on_each: impl FnMut(&str)) -> Result<Vec<String>, Refused> {
        let cli = self.cli.as_str();
        match &self.source {
            Source::Build { context, dockerfile } => {
                let mut args: Vec<String> = vec!["build".into(), "--quiet".into(), "-f".into(), dockerfile.display().to_string()];
                for t in &self.targets {
                    args.extend(["-t".into(), t.clone()]);
                }
                args.push(context.display().to_string());
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                run(cli, &args, None, None).await.map_err(|e| ("build", format!("build failed: {e}")))?;
            }
            Source::Local(image) => {
                for t in &self.targets {
                    run(cli, &["tag", image, t], None, None).await.map_err(|e| ("build", format!("tag {t}: {e}")))?;
                }
            }
        }
        let dir = match &login {
            Some(_) => Some(tempfile::Builder::new().prefix("hf-registry-").tempdir().map_err(|e| ("io", e.to_string()))?),
            None => None,
        };
        let config = dir.as_ref().map(tempfile::TempDir::path);
        if let (Some((user, token)), Some(c)) = (&login, config) {
            run(cli, &["login", self.registry.host(), "-u", user, "--password-stdin"], Some(c), Some(token))
                .await
                .map_err(|e| ("auth", format!("login to {}: {e}", self.registry.host())))?;
        }
        let mut done: Vec<String> = Vec::new();
        for t in &self.targets {
            if let Err(e) = run(cli, &["push", "--quiet", t], config, None).await {
                let left: Vec<&str> = self.targets.iter().map(String::as_str).filter(|x| !done.iter().any(|d| d == x)).collect();
                return Err(("push", format!("push {t}: {e}; pushed: [{}]; not pushed: [{}]", done.join(", "), left.join(", "))));
            }
            on_each(t);
            done.push(t.clone());
        }
        Ok(done)
    }
}

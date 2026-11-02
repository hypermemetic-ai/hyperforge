//! Open a GitHub pull request from a checkout's branch, on the token the
//! secret store holds for the repo's owner, or `github/_default`.

use std::path::{Path, PathBuf};

use crate::v5::secrets::{SecretRef, SecretResolver as _, YamlSecretStore};

pub struct Args {
    pub dir: PathBuf,
    pub title: String,
    pub body: String,
    pub base: Option<String>,
    pub draft: bool,
    pub dry_run: bool,
}

pub struct Opened {
    pub repo: String,
    pub head: String,
    pub base: String,
    pub title: String,
    pub dry_run: bool,
    pub number: Option<u64>,
    pub url: Option<String>,
}

type Refused = (&'static str, String);

fn api_root() -> String {
    std::env::var("HF_GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com".into())
}

fn token(config_dir: &Path, owner: &str) -> Result<String, Refused> {
    let store = YamlSecretStore::new(config_dir);
    [format!("secrets://github/{owner}/token"), "secrets://github/_default/token".to_string()]
        .iter()
        .find_map(|r| store.resolve(&SecretRef::parse(r).ok()?).ok())
        .ok_or_else(|| ("auth", format!("no token at secrets://github/{owner}/token or github/_default")))
}

fn api_error(what: &str, status: reqwest::StatusCode, body: &serde_json::Value) -> Refused {
    let detail = body["errors"][0]["message"].as_str().or_else(|| body["message"].as_str()).unwrap_or("");
    ("forge", format!("{what}: {status} {detail}"))
}

/// Open the pull request. Refuses a branch GitHub has not got in full.
pub async fn open(config_dir: &Path, a: Args) -> Result<Opened, Refused> {
    use crate::v5::ops::git;
    let g = |e: git::GitError| (e.code(), e.to_string());
    if a.title.is_empty() {
        return Err(("validation", "a pull request needs a title".into()));
    }
    let s = git::status(&a.dir).map_err(g)?;
    let head = s.branch.ok_or(("validation", "detached HEAD: check out a branch".to_string()))?;
    if s.upstream.is_none() {
        return Err(("unpushed", format!("{head} has no upstream: push it first (`repos push_refs`)")));
    }
    if s.ahead > 0 {
        return Err(("unpushed", format!("{head} is {} commits ahead of its upstream: push first", s.ahead)));
    }
    let origin = git::read_origin_url(&a.dir).map_err(g)?.ok_or(("validation", "no origin".to_string()))?;
    let (host, owner, name) = crate::v5::adapters::parse_remote_url(&origin)
        .ok_or_else(|| ("validation", format!("cannot read {origin}")))?;
    if host != "github.com" && !host.starts_with("127.0.0.1") {
        return Err(("unsupported", format!("{host}: pull requests are GitHub only")));
    }
    let tok = token(config_dir, &owner)?;
    let client = reqwest::Client::new();
    let url = format!("{}/repos/{owner}/{name}", api_root());
    let call = |r: reqwest::RequestBuilder| {
        r.bearer_auth(&tok)
            .header("User-Agent", "hyperforge")
            .header("Accept", "application/vnd.github+json")
            .send()
    };
    let net = |e: reqwest::Error| ("network", e.without_url().to_string());
    let base = if let Some(b) = a.base {
        b
    } else {
        let r = call(client.get(&url)).await.map_err(net)?;
        let status = r.status();
        let v: serde_json::Value = r.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(api_error("read repo", status, &v));
        }
        v["default_branch"].as_str().ok_or(("forge", "no default_branch".to_string()))?.to_string()
    };
    if base == head {
        return Err(("validation", format!("{head} is the base branch")));
    }
    let repo = format!("{owner}/{name}");
    if a.dry_run {
        return Ok(Opened { repo, head, base, title: a.title, dry_run: true, number: None, url: None });
    }
    let body = serde_json::json!({ "title": a.title, "head": head, "base": base, "body": a.body, "draft": a.draft });
    let r = call(client.post(format!("{url}/pulls")).json(&body)).await.map_err(net)?;
    let status = r.status();
    let v: serde_json::Value = r.json().await.unwrap_or_default();
    if status != reqwest::StatusCode::CREATED {
        return Err(api_error("open pull request", status, &v));
    }
    Ok(Opened {
        repo, head, base, title: a.title, dry_run: false,
        number: v["number"].as_u64(),
        url: v["html_url"].as_str().map(String::from),
    })
}

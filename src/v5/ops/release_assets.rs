//! Attach files to a forge release: GitHub and Codeberg. Everything that can
//! be refused is refused in [`plan`], before anything is written; files are
//! hashed and uploaded as streams, and tokens stay in the request headers.

use std::path::{Path, PathBuf};

use reqwest::{header, Body, Client, Method, RequestBuilder, Response, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;
use tokio_util::io::ReaderStream;

use crate::v5::secrets::{SecretRef, SecretResolver as _, YamlSecretStore};

pub struct Args {
    pub dir: PathBuf,
    pub tag: String,
    pub files: Vec<PathBuf>,
    pub replace: bool,
    pub create_release: bool,
    /// Where the forge answers, in place of its public hosts (tests).
    pub endpoint: Option<String>,
}

pub type Refused = (&'static str, String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Forge {
    GitHub,
    Codeberg,
}

impl Forge {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::Codeberg => "codeberg",
        }
    }
}

pub struct Asset {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub content_type: &'static str,
    /// The same-named asset already on the release, to replace.
    pub existing: Option<u64>,
}

pub struct Plan {
    pub repo: String,
    pub forge: Forge,
    pub tag: String,
    /// The release, or `None` when one will be created for the tag.
    pub release: Option<u64>,
    pub assets: Vec<Asset>,
    client: Client,
    api: String,
    uploads: String,
    token: String,
}

pub struct Uploaded {
    pub name: String,
    pub size: u64,
    pub url: Option<String>,
}

fn content_type(name: &str) -> &'static str {
    let is = |ends: &[&str]| ends.iter().any(|e| name.ends_with(e));
    if is(&[".tar.gz", ".tgz", ".gz"]) {
        "application/gzip"
    } else if is(&[".zip"]) {
        "application/zip"
    } else if is(&[".tar.xz", ".txz", ".xz"]) {
        "application/x-xz"
    } else if is(&[".tar.bz2", ".bz2"]) {
        "application/x-bzip2"
    } else if is(&[".txt", ".sha256", ".sha512", ".asc", ".sig"]) {
        "text/plain"
    } else if is(&[".json"]) {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

async fn describe(path: &Path) -> Result<Asset, Refused> {
    let shown = path.display();
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| ("validation", format!("{shown}: no file name")))?
        .to_string();
    if !path.is_file() {
        return Err(("not_found", format!("{shown}: no such file")));
    }
    let io = |e: std::io::Error| ("io", format!("{shown}: {e}"));
    let mut file = tokio::fs::File::open(path).await.map_err(io)?;
    let (mut hash, mut buf, mut size) = (Sha256::new(), vec![0u8; 64 * 1024], 0u64);
    loop {
        let n = file.read(&mut buf).await.map_err(io)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        size += n as u64;
    }
    let sha256 = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(Asset {
        content_type: content_type(&name),
        name,
        path: path.to_path_buf(),
        size,
        sha256,
        existing: None,
    })
}

fn token(config_dir: &Path, forge: Forge, owner: &str) -> Result<String, Refused> {
    let store = YamlSecretStore::new(config_dir);
    let f = forge.as_str();
    [format!("secrets://{f}/{owner}/token"), format!("secrets://{f}/_default/token")]
        .iter()
        .find_map(|r| store.resolve(&SecretRef::parse(r).ok()?).ok())
        .ok_or_else(|| ("auth", format!("no token at secrets://{f}/{owner}/token or {f}/_default")))
}

fn segments(tag: &str) -> String {
    tag.split('/').map(|s| urlencoding::encode(s).into_owned()).collect::<Vec<_>>().join("/")
}

fn net(e: reqwest::Error) -> Refused {
    ("network", e.without_url().to_string())
}

async fn refused(what: &str, r: Response) -> Refused {
    let status = r.status();
    let body: Value = r.json().await.unwrap_or_default();
    let detail = body["errors"][0]["message"].as_str().or_else(|| body["message"].as_str()).unwrap_or("");
    let code = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "auth",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY => "conflict",
        _ => "forge",
    };
    (code, format!("{what}: {status} {detail}"))
}

impl Plan {
    fn request(&self, method: Method, url: String) -> RequestBuilder {
        let auth = match self.forge {
            Forge::GitHub => format!("Bearer {}", self.token),
            Forge::Codeberg => format!("token {}", self.token),
        };
        let mut r = self
            .client
            .request(method, url)
            .header(header::AUTHORIZATION, auth)
            .header(header::USER_AGENT, "hyperforge");
        if self.forge == Forge::GitHub {
            r = r.header(header::ACCEPT, "application/vnd.github+json");
        }
        r
    }

    fn repo_url(&self) -> String {
        format!("{}/repos/{}", self.api, self.repo)
    }

    /// `Some` with the forge's answer, `None` on a 404.
    async fn get(&self, what: &str, url: String) -> Result<Option<Value>, Refused> {
        let r = self.request(Method::GET, url).send().await.map_err(net)?;
        match r.status() {
            StatusCode::NOT_FOUND => Ok(None),
            s if s.is_success() => Ok(Some(r.json().await.map_err(net)?)),
            _ => Err(refused(what, r).await),
        }
    }

    async fn existing(&self, release: u64) -> Result<Vec<(String, u64)>, Refused> {
        let mut found = Vec::new();
        for page in 1.. {
            let url = format!("{}/releases/{release}/assets?per_page=100&limit=100&page={page}", self.repo_url());
            let v = self.get("list assets", url).await?.unwrap_or_default();
            let items = v.as_array().cloned().unwrap_or_default();
            found.extend(items.iter().filter_map(|a| Some((a["name"].as_str()?.to_string(), a["id"].as_u64()?))));
            if items.len() < 100 {
                break;
            }
        }
        Ok(found)
    }

    /// The release to attach to; created if the plan says there is none.
    pub async fn ensure_release(&mut self) -> Result<u64, Refused> {
        if let Some(id) = self.release {
            return Ok(id);
        }
        let body = json!({ "tag_name": self.tag, "name": self.tag, "body": "", "draft": false, "prerelease": false });
        let r = self.request(Method::POST, format!("{}/releases", self.repo_url())).json(&body).send().await.map_err(net)?;
        if !r.status().is_success() {
            return Err(refused("create release", r).await);
        }
        let v: Value = r.json().await.map_err(net)?;
        let id = v["id"].as_u64().ok_or(("forge", "create release: no id in the answer".to_string()))?;
        self.release = Some(id);
        Ok(id)
    }

    async fn delete(&self, release: u64, asset: u64) -> Result<(), Refused> {
        let url = match self.forge {
            Forge::GitHub => format!("{}/releases/assets/{asset}", self.repo_url()),
            Forge::Codeberg => format!("{}/releases/{release}/assets/{asset}", self.repo_url()),
        };
        let r = self.request(Method::DELETE, url).send().await.map_err(net)?;
        if r.status().is_success() {
            Ok(())
        } else {
            Err(refused("delete the old asset", r).await)
        }
    }

    /// Upload one asset, deleting the one it replaces first (no forge here can
    /// swap in place): a failure after that leaves the release without it.
    pub async fn upload(&self, release: u64, asset: &Asset) -> Result<Uploaded, Refused> {
        if let Some(id) = asset.existing {
            self.delete(release, id).await?;
        }
        let after_delete = |e: Refused| {
            if asset.existing.is_some() {
                (e.0, format!("{}; the old {} is already deleted", e.1, asset.name))
            } else {
                e
            }
        };
        self.send(release, asset).await.map_err(after_delete)
    }

    /// Upload every asset in order, stopping at the first failure, which then
    /// names what reached the release and what did not.
    pub async fn upload_all(
        &self,
        release: u64,
        mut on_each: impl FnMut(&Asset, Uploaded),
    ) -> Result<Vec<String>, Refused> {
        let mut done: Vec<String> = Vec::new();
        for asset in &self.assets {
            match self.upload(release, asset).await {
                Ok(up) => {
                    on_each(asset, up);
                    done.push(asset.name.clone());
                }
                Err((code, message)) => {
                    let left: Vec<_> = self.assets.iter().map(|a| a.name.as_str())
                        .filter(|n| !done.iter().any(|d| d == n)).collect();
                    return Err((code, format!("{message}; uploaded: [{}]; not uploaded: [{}]", done.join(", "), left.join(", "))));
                }
            }
        }
        Ok(done)
    }

    async fn send(&self, release: u64, asset: &Asset) -> Result<Uploaded, Refused> {
        let file = tokio::fs::File::open(&asset.path)
            .await
            .map_err(|e| ("io", format!("{}: {e}", asset.path.display())))?;
        let body = Body::wrap_stream(ReaderStream::with_capacity(file, 64 * 1024));
        let name = urlencoding::encode(&asset.name);
        let request = match self.forge {
            Forge::GitHub => self
                .request(Method::POST, format!("{}/repos/{}/releases/{release}/assets?name={name}", self.uploads, self.repo))
                .header(header::CONTENT_TYPE, asset.content_type)
                .header(header::CONTENT_LENGTH, asset.size)
                .body(body),
            Forge::Codeberg => {
                let part = reqwest::multipart::Part::stream_with_length(body, asset.size)
                    .file_name(asset.name.clone())
                    .mime_str(asset.content_type)
                    .map_err(net)?;
                self.request(Method::POST, format!("{}/releases/{release}/assets?name={name}", self.repo_url()))
                    .multipart(reqwest::multipart::Form::new().part("attachment", part))
            }
        };
        let r = request.send().await.map_err(net)?;
        if !r.status().is_success() {
            return Err(refused(&format!("upload {}", asset.name), r).await);
        }
        let v: Value = r.json().await.map_err(net)?;
        let size = v["size"].as_u64();
        if size.is_some_and(|s| s != asset.size) {
            return Err(("forge", format!("{}: the forge holds {size:?} bytes of {}", asset.name, asset.size)));
        }
        Ok(Uploaded {
            name: asset.name.clone(),
            size: asset.size,
            url: v["browser_download_url"].as_str().map(String::from),
        })
    }
}

/// Check everything and read the release; writes nothing.
pub async fn plan(config_dir: &Path, a: Args) -> Result<Plan, Refused> {
    use crate::v5::ops::git;
    if a.tag.is_empty() {
        return Err(("validation", "a release asset needs a tag".into()));
    }
    if a.files.is_empty() {
        return Err(("validation", "no files to upload".into()));
    }
    let mut assets = Vec::with_capacity(a.files.len());
    for f in &a.files {
        assets.push(describe(f).await?);
    }
    for (i, x) in assets.iter().enumerate() {
        if assets[..i].iter().any(|y| y.name == x.name) {
            return Err(("validation", format!("two files are named {}", x.name)));
        }
    }

    let origin = git::read_origin_url(&a.dir)
        .map_err(|e| (e.code(), e.to_string()))?
        .ok_or(("validation", "no origin".to_string()))?;
    let (host, owner, name) = crate::v5::adapters::parse_remote_url(&origin)
        .ok_or_else(|| ("validation", format!("cannot read {origin}")))?;
    let forge = match host.as_str() {
        "github.com" => Forge::GitHub,
        "codeberg.org" => Forge::Codeberg,
        _ => return Err(("unsupported", format!("{host}: release assets are GitHub and Codeberg only"))),
    };
    let (api, uploads) = match (&a.endpoint, forge) {
        (Some(e), _) => (e.clone(), e.clone()),
        (None, Forge::GitHub) => ("https://api.github.com".into(), "https://uploads.github.com".into()),
        (None, Forge::Codeberg) => ("https://codeberg.org/api/v1".to_string(), String::new()),
    };
    let mut plan = Plan {
        repo: format!("{owner}/{name}"),
        forge,
        tag: a.tag,
        release: None,
        assets,
        client: Client::new(),
        api,
        uploads,
        token: token(config_dir, forge, &owner)?,
    };

    let tag = segments(&plan.tag);
    let tag_url = match forge {
        Forge::GitHub => format!("{}/git/ref/tags/{tag}", plan.repo_url()),
        Forge::Codeberg => format!("{}/tags/{tag}", plan.repo_url()),
    };
    if plan.get("read tag", tag_url).await?.is_none() {
        return Err(("not_found", format!("tag {} is not on {}: push it first", plan.tag, plan.repo)));
    }
    let release = plan.get("read release", format!("{}/releases/tags/{tag}", plan.repo_url())).await?;
    let Some(release) = release else {
        if a.create_release {
            return Ok(plan);
        }
        return Err((
            "not_found",
            format!("no release for {} on {}: pass --create-release true to create one", plan.tag, plan.repo),
        ));
    };
    let id = release["id"].as_u64().ok_or(("forge", "read release: no id in the answer".to_string()))?;
    plan.release = Some(id);
    let on_forge = plan.existing(id).await?;
    let mut clash = Vec::new();
    for asset in &mut plan.assets {
        asset.existing = on_forge.iter().find(|(n, _)| *n == asset.name).map(|(_, i)| *i);
        if asset.existing.is_some() {
            clash.push(asset.name.clone());
        }
    }
    if !clash.is_empty() && !a.replace {
        return Err(("conflict", format!("already on {}: {}; pass --replace true", plan.tag, clash.join(", "))));
    }
    Ok(plan)
}

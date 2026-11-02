//! `build.release_asset`, against a fake forge on a local socket speaking
//! GitHub's and Codeberg's release API. No real forge is reached.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use hyperforge::v5::ops::release_assets::{plan, Args, Forge};
use hyperforge::v5::service::HyperforgeService;
use plexus_core::plexus::{Activation, PlexusStreamItem};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct State {
    forge: Option<&'static str>,
    tag: bool,
    release: bool,
    assets: Vec<(u64, String, usize, String)>,
    next: u64,
    fail: Option<String>,
    writes: Vec<String>,
}

type Shared = Arc<Mutex<State>>;

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

fn answer(s: &mut State, method: &str, target: &str, head: &str, body: &[u8]) -> (u16, Value) {
    let path = target.split('?').next().unwrap();
    let rest = path.strip_prefix("/repos/acme/widget").unwrap_or("/?");
    let github = s.forge == Some("github");
    let want = if github { "authorization: bearer tok-gh" } else { "authorization: token tok-cb" };
    if !head.to_lowercase().contains(want) {
        return (401, json!({"message": "bad credentials"}));
    }
    let assets: Vec<Value> = s.assets.iter().map(|a| json!({"id": a.0, "name": a.1, "size": a.2})).collect();
    match (method, rest) {
        ("GET", "/git/ref/tags/v1") if github => (if s.tag { 200 } else { 404 }, json!({})),
        ("GET", "/tags/v1") if !github => (if s.tag { 200 } else { 404 }, json!({})),
        ("GET", "/releases/tags/v1") => (if s.release { 200 } else { 404 }, json!({"id": 7})),
        ("POST", "/releases") => {
            s.writes.push("create".into());
            s.release = true;
            (201, json!({"id": 7}))
        }
        ("GET", "/releases/7/assets") => (200, json!(assets)),
        ("POST", "/releases/7/assets") => {
            let name = target.split("name=").nth(1).unwrap_or("").to_string();
            s.writes.push(format!("upload {name}"));
            if s.fail.as_deref() == Some(name.as_str()) {
                return (500, json!({"message": "disk full"}));
            }
            let data = if github {
                assert!(head.to_lowercase().contains("content-type: application/gzip"), "{head}");
                body.to_vec()
            } else {
                assert!(find(body, b"name=\"attachment\"").is_some());
                let start = find(body, b"\r\n\r\n").unwrap() + 4;
                body[start..rfind(body, b"\r\n--").unwrap()].to_vec()
            };
            s.next += 1;
            let id = 100 + s.next;
            s.assets.push((id, name.clone(), data.len(), String::from_utf8_lossy(&data).into_owned()));
            (201, json!({"id": id, "name": name, "size": data.len(), "browser_download_url": format!("https://dl/{name}")}))
        }
        ("DELETE", _) => {
            let id: u64 = rest.rsplit('/').next().unwrap().parse().unwrap();
            let right = if github { format!("/releases/assets/{id}") } else { format!("/releases/7/assets/{id}") };
            assert_eq!(rest, right);
            s.writes.push(format!("delete {id}"));
            s.assets.retain(|a| a.0 != id);
            (204, Value::Null)
        }
        _ => panic!("the fake has no {method} {target}"),
    }
}

async fn fake(state: Shared) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let state = state.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let end = loop {
                    if let Some(e) = find(&buf, b"\r\n\r\n") {
                        break e;
                    }
                    let n = sock.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                };
                let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                    .unwrap_or(0);
                while buf.len() < end + 4 + len {
                    let n = sock.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "the body ended short of content-length");
                    buf.extend_from_slice(&chunk[..n]);
                }
                let mut parts = head.split_whitespace();
                let (method, target) = (parts.next().unwrap().to_string(), parts.next().unwrap().to_string());
                let (code, value) = answer(&mut state.lock().unwrap(), &method, &target, &head, &buf[end + 4..]);
                let text = if value.is_null() { String::new() } else { value.to_string() };
                let reply = format!(
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
                sock.write_all(reply.as_bytes()).await.unwrap();
            });
        }
    });
    url
}

struct Rig {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    repo: PathBuf,
    state: Shared,
    url: String,
    forge: Forge,
}

async fn rig(forge: Forge) -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, config) = (tmp.path().join("widget"), tmp.path().join("config"));
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    let host = if forge == Forge::GitHub { "github.com" } else { "codeberg.org" };
    let run = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&repo).args(args).status().unwrap().success());
    run(&["init", "-q"]);
    run(&["remote", "add", "origin", &format!("https://{host}/acme/widget.git")]);
    std::fs::write(repo.join("a.tar.gz"), "alpha").unwrap();
    std::fs::write(repo.join("b.tar.gz"), "bravo!").unwrap();
    std::fs::write(
        config.join("secrets.yaml"),
        "github/_default/token: tok-gh
codeberg/acme/token: tok-cb
",
    )
    .unwrap();
    let state = Shared::default();
    {
        let mut s = state.lock().unwrap();
        s.forge = Some(forge.as_str());
        s.tag = true;
        s.release = true;
    }
    let url = fake(state.clone()).await;
    Rig { _tmp: tmp, config, repo, state, url, forge }
}

impl Rig {
    fn args(&self, files: &[&str]) -> Args {
        Args {
            dir: self.repo.clone(),
            tag: "v1".into(),
            files: files.iter().map(|f| self.repo.join(f)).collect(),
            replace: false,
            create_release: false,
            endpoint: Some(self.url.clone()),
        }
    }

    fn writes(&self) -> Vec<String> {
        self.state.lock().unwrap().writes.clone()
    }
}

fn code<T>(r: Result<T, (&'static str, String)>) -> (&'static str, String) {
    r.err().expect("refused")
}

async fn both<F, Fut>(f: F)
where
    F: Fn(Rig) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    f(rig(Forge::GitHub).await).await;
    f(rig(Forge::Codeberg).await).await;
}

fn seed(s: &Shared) {
    s.lock().unwrap().assets.push((55, "a.tar.gz".into(), 3, "old".into()));
}

#[tokio::test]
async fn a_plan_reports_size_and_sha256_and_writes_nothing() {
    both(|r| async move {
        let p = plan(&r.config, r.args(&["a.tar.gz"])).await.unwrap();
        assert_eq!(p.forge, r.forge);
        assert_eq!((p.repo.as_str(), p.tag.as_str(), p.release), ("acme/widget", "v1", Some(7)));
        assert_eq!(p.assets[0].size, 5);
        assert_eq!(p.assets[0].sha256, "8ed3f6ad685b959ead7022518e1af76cd816f8e8ec7ccdda1ed4018e8f2223f8");
        assert_eq!(r.writes(), Vec::<String>::new());
    })
    .await;
}

#[tokio::test]
async fn files_stream_up_with_their_bytes() {
    both(|r| async move {
        let p = plan(&r.config, r.args(&["a.tar.gz", "b.tar.gz"])).await.unwrap();
        for a in &p.assets {
            let up = p.upload(7, a).await.unwrap();
            assert_eq!(up.url.unwrap(), format!("https://dl/{}", a.name));
        }
        let s = r.state.lock().unwrap();
        let held: Vec<_> = s.assets.iter().map(|a| (a.1.as_str(), a.3.as_str())).collect();
        assert_eq!(held, [("a.tar.gz", "alpha"), ("b.tar.gz", "bravo!")]);
    })
    .await;
}

#[tokio::test]
async fn a_missing_file_is_refused_before_the_forge_is_asked() {
    both(|mut r| async move {
        r.url = "http://127.0.0.1:1".into();
        let e = code(plan(&r.config, r.args(&["a.tar.gz", "gone.tar.gz"])).await);
        assert_eq!(e.0, "not_found");
        assert!(e.1.contains("gone.tar.gz"), "{e:?}");
    })
    .await;
}

#[tokio::test]
async fn a_tag_not_on_the_remote_is_refused() {
    both(|r| async move {
        r.state.lock().unwrap().tag = false;
        let mut a = r.args(&["a.tar.gz"]);
        a.create_release = true;
        let e = code(plan(&r.config, a).await);
        assert_eq!(e.0, "not_found");
        assert!(e.1.contains("push it first"), "{e:?}");
    })
    .await;
}

#[tokio::test]
async fn no_release_is_refused_unless_asked_to_create_one() {
    both(|r| async move {
        r.state.lock().unwrap().release = false;
        let e = code(plan(&r.config, r.args(&["a.tar.gz"])).await);
        assert!(e.1.contains("--create-release"), "{e:?}");
        let mut a = r.args(&["a.tar.gz"]);
        a.create_release = true;
        let mut p = plan(&r.config, a).await.unwrap();
        assert_eq!((p.release, r.writes().len()), (None, 0));
        assert_eq!(p.ensure_release().await.unwrap(), 7);
        assert_eq!(r.writes(), ["create"]);
    })
    .await;
}

#[tokio::test]
async fn an_existing_asset_stays_unless_replace_is_asked() {
    both(|r| async move {
        seed(&r.state);
        let e = code(plan(&r.config, r.args(&["b.tar.gz", "a.tar.gz"])).await);
        assert_eq!(e.0, "conflict");
        assert!(e.1.contains("a.tar.gz") && !e.1.contains("b.tar.gz"), "{e:?}");
        assert_eq!(r.writes().len(), 0);

        let mut a = r.args(&["a.tar.gz"]);
        a.replace = true;
        let p = plan(&r.config, a).await.unwrap();
        assert_eq!(p.assets[0].existing, Some(55));
        p.upload(7, &p.assets[0]).await.unwrap();
        assert_eq!(r.writes(), ["delete 55", "upload a.tar.gz"]);
        assert_eq!(r.state.lock().unwrap().assets[0].3, "alpha");
    })
    .await;
}

#[tokio::test]
async fn a_failed_replace_says_the_old_asset_is_gone() {
    both(|r| async move {
        seed(&r.state);
        r.state.lock().unwrap().fail = Some("a.tar.gz".into());
        let mut a = r.args(&["a.tar.gz"]);
        a.replace = true;
        let p = plan(&r.config, a).await.unwrap();
        let e = code(p.upload(7, &p.assets[0]).await);
        assert!(e.1.contains("disk full") && e.1.contains("already deleted"), "{e:?}");
    })
    .await;
}

#[tokio::test]
async fn a_token_is_never_in_a_refusal() {
    both(|r| async move {
        let wrong = "github/_default/token: wrong-secret
codeberg/acme/token: wrong-secret
";
        std::fs::write(r.config.join("secrets.yaml"), wrong).unwrap();
        let e = code(plan(&r.config, r.args(&["a.tar.gz"])).await);
        assert_eq!(e.0, "auth");
        assert!(!e.1.contains("wrong-secret"), "{e:?}");
    })
    .await;
}

async fn wire(config: &Path, params: Value) -> Vec<Value> {
    let service = HyperforgeService::new(config.to_path_buf());
    let mut stream = service.call("build.release_asset", params, None, None).await.unwrap();
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        if let PlexusStreamItem::Data { content, .. } = item {
            events.push(content.into_value());
        }
    }
    events
}

#[tokio::test]
async fn over_the_wire_a_refusal_is_a_typed_error() {
    let r = rig(Forge::GitHub).await;
    let events = wire(
        &r.config,
        json!({"path": r.repo, "tag": "v1", "files": [r.repo.join("gone.tar.gz")], "execute": true}),
    )
    .await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!((events[0]["type"].as_str(), events[0]["code"].as_str()), (Some("error"), Some("not_found")));
}

#[tokio::test]
async fn a_failure_part_way_names_what_uploaded() {
    both(|r| async move {
        r.state.lock().unwrap().fail = Some("b.tar.gz".into());
        let p = plan(&r.config, r.args(&["a.tar.gz", "b.tar.gz"])).await.unwrap();
        let mut seen = Vec::new();
        let e = code(p.upload_all(7, |a, _| seen.push(a.name.clone())).await);
        assert_eq!(seen, ["a.tar.gz"]);
        assert!(e.1.contains("uploaded: [a.tar.gz]") && e.1.contains("not uploaded: [b.tar.gz]"), "{e:?}");
    })
    .await;
}

/// The only test that sets `HF_RELEASE_API_URL`; the wire test above is
/// refused before it would be read.
#[tokio::test]
async fn over_the_wire_it_lists_by_default_and_uploads_on_execute() {
    let r = rig(Forge::GitHub).await;
    std::env::set_var("HF_RELEASE_API_URL", &r.url);
    let params = |execute: bool| json!({"path": r.repo, "tag": "v1", "files": [r.repo.join("a.tar.gz")], "execute": execute});

    let dry = wire(&r.config, params(false)).await;
    assert_eq!(dry[0]["type"], "release_asset", "{dry:?}");
    assert_eq!((dry[0]["size"].as_u64(), dry[0]["uploaded"].as_bool(), dry[0]["dry_run"].as_bool()), (Some(5), Some(false), Some(true)));
    assert_eq!(dry[0]["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(dry[1]["type"], "release_assets_done");
    assert_eq!(r.writes().len(), 0);

    let done = wire(&r.config, params(true)).await;
    assert_eq!(done[0]["uploaded"], true, "{done:?}");
    assert_eq!(done[0]["url"], "https://dl/a.tar.gz");
    assert_eq!(done[1]["uploaded"], json!(["a.tar.gz"]));
    assert_eq!(r.writes(), ["upload a.tar.gz"]);
}

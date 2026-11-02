//! `images.*`, against a fake registry API on a local socket (GitHub's and
//! Codeberg's packages endpoints) and a fake container CLI that records its
//! arguments and stdin. No real registry is reached.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use hyperforge::v5::ops::images::{plan_delete, plan_push, Api, PushArgs, Registry, Version};
use hyperforge::v5::service::HyperforgeService;
use plexus_core::plexus::{Activation, PlexusStreamItem};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const GH_TOKEN: &str = "tok-gh-secret";
const CB_TOKEN: &str = "tok-cb-secret";

#[derive(Default)]
struct State {
    github: bool,
    /// GitHub: the owner is a person, so `/orgs/` answers 404.
    person: bool,
    writes: Vec<String>,
}

type Shared = Arc<Mutex<State>>;

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn gh_versions() -> Value {
    json!([
        {"id": 11, "name": "sha256:aaa", "created_at": "2026-11-01T00:00:00Z", "metadata": {"container": {"tags": ["v2", "latest"]}}},
        {"id": 12, "name": "sha256:bbb", "created_at": "2026-10-01T00:00:00Z", "metadata": {"container": {"tags": ["v1"]}}},
        {"id": 13, "name": "sha256:ccc", "created_at": "2026-10-01T00:00:00Z", "metadata": {"container": {"tags": []}}}
    ])
}

fn cb_packages() -> Value {
    json!([
        {"id": 21, "name": "app", "version": "v2", "created_at": "2026-11-01T00:00:00Z"},
        {"id": 22, "name": "app", "version": "latest", "created_at": "2026-11-01T00:00:00Z"},
        {"id": 23, "name": "app", "version": "sha256:ddd", "created_at": "2026-11-01T00:00:00Z"},
        {"id": 24, "name": "other", "version": "v9", "created_at": "2026-09-01T00:00:00Z"}
    ])
}

fn answer(s: &mut State, method: &str, target: &str, head: &str) -> (u16, Value) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let want = if s.github { format!("authorization: bearer {GH_TOKEN}") } else { format!("authorization: token {CB_TOKEN}") };
    if !head.to_lowercase().contains(&want) {
        return (401, json!({"message": "bad credentials"}));
    }
    let first_page = query.contains("page=1");
    if s.github {
        let root = if s.person { "/users/acme" } else { "/orgs/acme" };
        if path == "/user" {
            return (200, json!({"login": "bot"}));
        }
        let Some(rest) = path.strip_prefix(root) else { return (404, json!({"message": "Not Found"})) };
        return match (method, rest) {
            ("GET", "/packages") => (200, if first_page { json!([{"name": "app", "visibility": "public", "created_at": "2026-01-01T00:00:00Z"}]) } else { json!([]) }),
            ("GET", "/packages/container/app/versions") => (200, if first_page { gh_versions() } else { json!([]) }),
            ("GET", _) => (404, json!({"message": "Package not found."})),
            ("DELETE", r) => {
                s.writes.push(format!("delete {r}"));
                (204, Value::Null)
            }
            _ => panic!("the fake has no {method} {target}"),
        };
    }
    match (method, path) {
        ("GET", "/user") => (200, json!({"login": "cbbot"})),
        ("GET", "/packages/acme") => {
            let all = cb_packages();
            let items: Vec<Value> = match query.split('&').find_map(|kv| kv.strip_prefix("q=")) {
                Some(q) => all.as_array().unwrap().iter().filter(|p| p["name"] == q).cloned().collect(),
                None => all.as_array().unwrap().clone(),
            };
            (200, if first_page { json!(items) } else { json!([]) })
        }
        ("DELETE", p) => {
            s.writes.push(format!("delete {p}"));
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
                let mut parts = head.split_whitespace();
                let (method, target) = (parts.next().unwrap().to_string(), parts.next().unwrap().to_string());
                let (code, value) = answer(&mut state.lock().unwrap(), &method, &target, &head);
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
    tmp: tempfile::TempDir,
    config: PathBuf,
    state: Shared,
    url: String,
}

async fn rig(github: bool) -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config");
    std::fs::create_dir_all(config.join("orgs")).unwrap();
    std::fs::create_dir_all(config.join("workspaces")).unwrap();
    let provider = if github { "github" } else { "codeberg" };
    std::fs::write(
        config.join("orgs/acme.yaml"),
        format!("name: acme\nforge:\n  provider: {provider}\n  credentials: []\nrepos: []\n"),
    )
    .unwrap();
    std::fs::write(
        config.join("secrets.yaml"),
        format!("github/acme/packages_token: {GH_TOKEN}\ncodeberg/_default/token: {CB_TOKEN}\n"),
    )
    .unwrap();
    let state: Shared = Arc::new(Mutex::new(State { github, ..State::default() }));
    let url = fake(state.clone()).await;
    Rig { tmp, config, state, url }
}

impl Rig {
    fn api(&self, registry: Registry) -> Api {
        Api::new(&self.config, registry, "acme", Some(self.url.clone()))
    }
    fn writes(&self) -> Vec<String> {
        self.state.lock().unwrap().writes.clone()
    }
}

/// A container CLI that logs each call's arguments, and a login's stdin,
/// to `log`; it fails a push of `fail`.
fn fake_cli(dir: &Path, fail: &str) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;
    let log = dir.join("cli.log");
    let cli = dir.join("fake-docker");
    let script = format!(
        r#"#!/bin/sh
log='{log}'
printf 'ARGS' >> "$log"; for a in "$@"; do printf ' %s' "$a" >> "$log"; done; echo >> "$log"
if [ "$1" = "--config" ]; then [ -d "$2" ] && echo "CONFIG-EXISTS $2" >> "$log"; fi
case " $* " in *" login "*) printf 'STDIN ' >> "$log"; cat >> "$log"; echo >> "$log";; esac
case " $* " in *" push "*"{fail}"*) echo "denied: no access" >&2; exit 1;; esac
exit 0
"#,
        log = log.display()
    );
    std::fs::write(&cli, script).unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    (cli, log)
}

fn push_args(r: &Rig, cli: &Path, tags: &[&str]) -> PushArgs {
    let ctx = r.tmp.path().join("ctx");
    std::fs::create_dir_all(&ctx).unwrap();
    std::fs::write(ctx.join("Dockerfile"), "FROM scratch\n").unwrap();
    PushArgs {
        owner: "acme".into(),
        registry: Registry::Ghcr,
        name: "app".into(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        context: Some(ctx),
        dockerfile: None,
        image: None,
        cli: Some(cli.display().to_string()),
    }
}

#[tokio::test]
async fn github_lists_versions_with_their_tags() {
    let r = rig(true).await;
    let v = r.api(Registry::Ghcr).versions("app").await.unwrap();
    assert_eq!(v.len(), 3);
    assert_eq!(v[0].tags, ["v2", "latest"]);
    assert_eq!((v[0].id, v[0].digest.as_deref()), (11, Some("sha256:aaa")));
    assert!(v[2].tags.is_empty());
    assert_eq!(r.api(Registry::Ghcr).packages().await.unwrap()[0].name, "app");
}

#[tokio::test]
async fn github_falls_back_to_a_persons_packages() {
    let r = rig(true).await;
    r.state.lock().unwrap().person = true;
    assert_eq!(r.api(Registry::Ghcr).versions("app").await.unwrap().len(), 3);
    assert_eq!(r.api(Registry::Ghcr).versions("nope").await.unwrap().len(), 0, "no image is an empty list");
}

#[tokio::test]
async fn codeberg_lists_one_images_tags_and_digests() {
    let r = rig(false).await;
    let v = r.api(Registry::Codeberg).versions("app").await.unwrap();
    let tags: Vec<_> = v.iter().flat_map(|x| x.tags.clone()).collect();
    assert_eq!(tags, ["v2", "latest"]);
    assert_eq!(v[2].digest.as_deref(), Some("sha256:ddd"));
    let names: Vec<_> = r.api(Registry::Codeberg).packages().await.unwrap().into_iter().map(|p| p.name).collect();
    assert_eq!(names, ["app", "other"]);
}

#[tokio::test]
async fn a_delete_that_takes_other_tags_is_refused_unless_asked() {
    let r = rig(true).await;
    let v: Vec<Version> = r.api(Registry::Ghcr).versions("app").await.unwrap();
    let e = plan_delete(&v, "app", "latest", false).err().unwrap();
    assert_eq!(e.0, "conflict");
    assert!(e.1.contains("v2"), "{e:?}");
    let p = plan_delete(&v, "app", "latest", true).unwrap();
    assert_eq!((p.version.id, p.other_tags.clone()), (11, vec!["v2".to_string()]));
    assert_eq!(plan_delete(&v, "app", "v1", false).unwrap().version.id, 12);
    assert_eq!(plan_delete(&v, "app", "sha256:ccc", false).unwrap().version.id, 13);
    assert_eq!(plan_delete(&v, "app", "v9", false).err().unwrap().0, "not_found");
}

#[tokio::test]
async fn delete_removes_the_named_version_on_each_forge() {
    let r = rig(true).await;
    let api = r.api(Registry::Ghcr);
    let v = api.versions("app").await.unwrap();
    let p = plan_delete(&v, "app", "v1", false).unwrap();
    api.delete("app", &p.version, &p.by).await.unwrap();
    assert_eq!(r.writes(), ["delete /packages/container/app/versions/12"]);

    let r = rig(false).await;
    let api = r.api(Registry::Codeberg);
    let v = api.versions("app").await.unwrap();
    let p = plan_delete(&v, "app", "v2", false).unwrap();
    api.delete("app", &p.version, &p.by).await.unwrap();
    assert_eq!(r.writes(), ["delete /packages/acme/container/app/v2"]);
}

#[tokio::test]
async fn a_token_is_never_in_a_refusal() {
    let r = rig(true).await;
    std::fs::write(r.config.join("secrets.yaml"), "github/acme/token: wrong-secret\n").unwrap();
    let e = r.api(Registry::Ghcr).versions("app").await.err().unwrap();
    assert_eq!(e.0, "auth");
    assert!(!e.1.contains("wrong-secret"), "{e:?}");
}

#[tokio::test]
async fn a_push_is_checked_before_anything_runs() {
    let r = rig(true).await;
    let (cli, log) = fake_cli(r.tmp.path(), "none");
    assert_eq!(plan_push(push_args(&r, &cli, &["Bad Tag"])).await.err().unwrap().0, "validation");
    let mut a = push_args(&r, &cli, &["v1"]);
    a.image = Some("app:v1".into());
    assert_eq!(plan_push(a).await.err().unwrap().0, "validation", "path and image both");
    let mut a = push_args(&r, &cli, &["v1"]);
    a.name = "App".into();
    assert_eq!(plan_push(a).await.err().unwrap().0, "validation");
    let mut a = push_args(&r, &cli, &["v1"]);
    a.dockerfile = Some("Nope".into());
    assert_eq!(plan_push(a).await.err().unwrap().0, "not_found");
    let p = plan_push(push_args(&r, &cli, &["v1", "latest"])).await.unwrap();
    assert_eq!(p.targets, ["ghcr.io/acme/app:v1", "ghcr.io/acme/app:latest"]);
    assert!(!log.exists(), "planning a build runs nothing");
}

#[tokio::test]
async fn a_push_signs_in_through_a_throwaway_config_on_stdin() {
    let r = rig(true).await;
    let (cli, log) = fake_cli(r.tmp.path(), "none");
    let p = plan_push(push_args(&r, &cli, &["v1", "latest"])).await.unwrap();
    let mut seen = Vec::new();
    let done = p.execute(Some(("bot".into(), GH_TOKEN.into())), |t| seen.push(t.to_string())).await.unwrap();
    assert_eq!(done, ["ghcr.io/acme/app:v1", "ghcr.io/acme/app:latest"]);
    assert_eq!(seen, done);
    let text = std::fs::read_to_string(&log).unwrap();
    let args: Vec<&str> = text.lines().filter(|l| l.starts_with("ARGS")).collect();
    assert!(args[0].contains("build --quiet -f") && args[0].contains("-t ghcr.io/acme/app:v1 -t ghcr.io/acme/app:latest"), "{text}");
    assert!(args[1].contains("login ghcr.io -u bot --password-stdin") && args[1].starts_with("ARGS --config "), "{text}");
    assert!(args[2].contains("push --quiet ghcr.io/acme/app:v1") && args[2].starts_with("ARGS --config "), "{text}");
    assert!(text.contains(&format!("STDIN {GH_TOKEN}")), "the token goes on stdin: {text}");
    assert!(args.iter().all(|a| !a.contains(GH_TOKEN)), "never in arguments: {text}");
    assert!(text.contains("CONFIG-EXISTS"), "{text}");
    let config = args[1].split_whitespace().nth(2).unwrap();
    assert!(!Path::new(config).exists(), "the throwaway config is gone after: {config}");
}

#[tokio::test]
async fn without_a_token_a_push_uses_the_clis_own_login() {
    let r = rig(true).await;
    let (cli, log) = fake_cli(r.tmp.path(), "none");
    let mut a = push_args(&r, &cli, &["v1"]);
    a.context = None;
    a.image = Some("app:built".into());
    let p = plan_push(a).await.unwrap();
    p.execute(None, |_| {}).await.unwrap();
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("ARGS image inspect"), "{text}");
    assert!(text.contains("ARGS tag app:built ghcr.io/acme/app:v1"), "{text}");
    assert!(text.contains("ARGS push --quiet ghcr.io/acme/app:v1"), "{text}");
    assert!(!text.contains("login") && !text.contains("--config"), "{text}");
}

#[tokio::test]
async fn a_failed_push_names_what_was_pushed() {
    let r = rig(true).await;
    let (cli, _log) = fake_cli(r.tmp.path(), "app:latest");
    let p = plan_push(push_args(&r, &cli, &["v1", "latest"])).await.unwrap();
    let e = p.execute(Some(("bot".into(), GH_TOKEN.into())), |_| {}).await.err().unwrap();
    assert_eq!(e.0, "push");
    assert!(e.1.contains("pushed: [ghcr.io/acme/app:v1]") && e.1.contains("not pushed: [ghcr.io/acme/app:latest]"), "{e:?}");
    assert!(e.1.contains("denied") && !e.1.contains(GH_TOKEN), "{e:?}");
}

async fn wire(config: &Path, method: &str, params: Value) -> Vec<Value> {
    let service = HyperforgeService::new(config.to_path_buf());
    let mut stream = service.call(method, params, None, None).await.unwrap();
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        if let PlexusStreamItem::Data { content, .. } = item {
            events.push(content.into_value());
        }
    }
    events
}

/// The only test that sets `HF_REGISTRY_API_URL` and `HF_CONTAINER_CLI`.
#[tokio::test]
async fn over_the_wire_push_and_delete_only_plan_until_execute() {
    let r = rig(true).await;
    let (cli, log) = fake_cli(r.tmp.path(), "none");
    std::env::set_var("HF_REGISTRY_API_URL", &r.url);
    std::env::set_var("HF_CONTAINER_CLI", &cli);

    let listed = wire(&r.config, "images.list", json!({"org": "acme", "name": "app", "filter": "v"})).await;
    assert_eq!(listed.len(), 3, "{listed:?}");
    assert_eq!((listed[0]["type"].as_str(), listed[0]["image"].as_str()), (Some("image_version"), Some("ghcr.io/acme/app")));
    assert_eq!(listed[2]["count"], 2);

    let all = wire(&r.config, "images.list_all", json!({"org": "acme"})).await;
    assert_eq!((all[0]["name"].as_str(), all[1]["count"].as_u64()), (Some("app"), Some(1)), "{all:?}");

    let dry = wire(&r.config, "images.delete", json!({"org": "acme", "name": "app", "tag": "v1"})).await;
    assert_eq!((dry[0]["type"].as_str(), dry[0]["deleted"].as_bool(), dry[0]["dry_run"].as_bool()), (Some("image_delete"), Some(false), Some(true)), "{dry:?}");
    assert!(r.writes().is_empty());
    let refused = wire(&r.config, "images.delete", json!({"org": "acme", "name": "app", "tag": "latest", "execute": true})).await;
    assert_eq!((refused[0]["type"].as_str(), refused[0]["code"].as_str()), (Some("error"), Some("conflict")));
    assert!(r.writes().is_empty());
    let done = wire(&r.config, "images.delete", json!({"org": "acme", "name": "app", "tag": "v1", "execute": true})).await;
    assert_eq!(done[0]["deleted"], true, "{done:?}");
    assert_eq!(r.writes(), ["delete /packages/container/app/versions/12"]);

    let ctx = r.tmp.path().join("wire-ctx");
    std::fs::create_dir_all(&ctx).unwrap();
    std::fs::write(ctx.join("Containerfile"), "FROM scratch\n").unwrap();
    let push = |execute: bool| json!({"org": "acme", "name": "app", "tag": "v3", "also_tags": ["latest"], "path": ctx, "execute": execute});
    let planned = wire(&r.config, "images.push", push(false)).await;
    assert_eq!(planned.len(), 2, "{planned:?}");
    assert_eq!((planned[0]["pushed"].as_bool(), planned[0]["dry_run"].as_bool()), (Some(false), Some(true)));
    assert!(planned[0]["source"].as_str().unwrap().contains("Containerfile"));
    assert!(!log.exists(), "a planned push runs nothing");
    let pushed = wire(&r.config, "images.push", push(true)).await;
    assert_eq!(pushed.len(), 2, "{pushed:?}");
    assert_eq!((pushed[1]["image"].as_str(), pushed[1]["pushed"].as_bool()), (Some("ghcr.io/acme/app:latest"), Some(true)));
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("login ghcr.io -u bot --password-stdin"), "the user is the token's: {text}");

    let unknown = wire(&r.config, "images.list", json!({"org": "nobody", "name": "app"})).await;
    assert_eq!((unknown[0]["type"].as_str(), unknown[0]["code"].as_str()), (Some("error"), Some("not_found")));
    let gitlab = wire(&r.config, "images.list", json!({"org": "acme", "name": "app", "forge": "gitlab"})).await;
    assert_eq!(gitlab[0]["code"], "unsupported");
}

//! `repos.mirror` (HYPERFORGE-60): two fake forges are local bare repos,
//! tagged `github` and `codeberg` on the org yaml. Nothing leaves the box.

use std::path::{Path, PathBuf};
use std::process::Command;

use futures::StreamExt;
use hyperforge::v5::service::HyperforgeService;
use plexus_core::plexus::{Activation, PlexusStreamItem};
use serde_json::{json, Value};

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn sha(bare: &Path, r: &str) -> Option<String> {
    let o = Command::new("git").arg("-C").arg(bare).args(["rev-parse", "--verify", "--quiet", r]).output().unwrap();
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

struct World {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    ws: PathBuf,
    gh: PathBuf,
    cb: PathBuf,
}

/// A workspace with `widget` (and `gadget`), each registered on github and
/// codeberg (bare repos), on branch `main` with tag `v1`.
fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let (config, ws, forges) = (tmp.path().join("config"), tmp.path().join("ws"), tmp.path().join("forges"));
    for d in [&config.join("orgs"), &config.join("workspaces"), &ws, &forges] {
        std::fs::create_dir_all(d).unwrap();
    }
    let mut repos_yaml = String::new();
    for name in ["widget", "gadget"] {
        let dir = ws.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "one"]);
        git(&dir, &["tag", "v1"]);
        for f in ["gh", "cb"] {
            let bare = forges.join(format!("{name}-{f}.git"));
            git(&forges, &["init", "-q", "--bare", bare.to_str().unwrap()]);
        }
        repos_yaml.push_str(&format!(
            "- name: {name}\n  remotes:\n  - url: {gh}\n    provider: github\n  - url: {cb}\n    provider: codeberg\n",
            gh = forges.join(format!("{name}-gh.git")).display(),
            cb = forges.join(format!("{name}-cb.git")).display(),
        ));
    }
    std::fs::write(config.join("config.yaml"), "provider_map:\n  github.com: github\n  codeberg.org: codeberg\n").unwrap();
    std::fs::write(
        config.join("orgs/acme.yaml"),
        format!("name: acme\nforge:\n  provider: github\nrepos:\n{repos_yaml}"),
    )
    .unwrap();
    std::fs::write(
        config.join("workspaces/main.yaml"),
        format!("name: main\npath: {}\nrepos:\n- acme/widget\n- acme/gadget\n", ws.display()),
    )
    .unwrap();
    World { gh: forges.join("widget-gh.git"), cb: forges.join("widget-cb.git"), _tmp: tmp, config, ws }
}

async fn mirror(config: &Path, params: Value) -> Vec<Value> {
    let service = HyperforgeService::new(config.to_path_buf());
    let mut stream = service.call("repos.mirror", params, None, None).await.unwrap();
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        if let PlexusStreamItem::Data { content, .. } = item {
            events.push(content.into_value());
        }
    }
    events
}

fn of<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["type"] == kind).collect()
}

fn one(_: &World) -> Value {
    json!({"workspace": "main", "name": "widget"})
}

#[tokio::test]
async fn the_default_is_a_plan_and_pushes_nothing() {
    let w = world();
    let events = mirror(&w.config, one(&w)).await;
    let plans = of(&events, "mirror_plan");
    assert_eq!(plans.len(), 2, "{events:?}");
    assert_eq!(plans[0]["provider"], "github");
    assert_eq!(plans[1]["provider"], "codeberg");
    assert_eq!(plans[0]["refspecs"], json!(["refs/heads/main:refs/heads/main", "refs/tags/*:refs/tags/*"]));
    assert_eq!(plans[0]["auth"], "local");
    assert_eq!(of(&events, "mirror_summary")[0]["dry_run"], true);
    assert_eq!((sha(&w.gh, "refs/heads/main"), sha(&w.cb, "refs/heads/main")), (None, None));
}

#[tokio::test]
async fn execute_pushes_the_branch_and_tags_to_every_forge() {
    let w = world();
    let mut p = one(&w);
    p["execute"] = json!(true);
    let events = mirror(&w.config, p).await;
    assert_eq!(of(&events, "mirror_pushed").len(), 2, "{events:?}");
    assert!(of(&events, "error").is_empty(), "{events:?}");
    let head = git(&w.ws.join("widget"), &["rev-parse", "HEAD"]);
    for bare in [&w.gh, &w.cb] {
        assert_eq!(sha(bare, "refs/heads/main").as_deref(), Some(head.as_str()));
        assert!(sha(bare, "refs/tags/v1").is_some());
    }
    // A second run finds both up to date.
    let mut p = one(&w);
    p["execute"] = json!(true);
    let again = mirror(&w.config, p).await;
    for e in of(&again, "mirror_pushed") {
        assert_eq!(e["updated"], json!([]), "{again:?}");
    }
}

#[tokio::test]
async fn a_forge_that_moved_on_is_refused_not_forced_and_named() {
    let w = world();
    // codeberg's main holds a commit the checkout lacks.
    let other = w._tmp.path().join("other");
    git(w._tmp.path(), &["clone", "-q", w.cb.to_str().unwrap(), other.to_str().unwrap()]);
    git(&other, &["checkout", "-q", "-b", "main"]);
    git(&other, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "theirs"]);
    git(&other, &["push", "-q", "origin", "main"]);
    let theirs = sha(&w.cb, "refs/heads/main").unwrap();

    let mut p = one(&w);
    p["execute"] = json!(true);
    let events = mirror(&w.config, p).await;
    assert_eq!(sha(&w.cb, "refs/heads/main").as_deref(), Some(theirs.as_str()), "never forced");
    assert!(sha(&w.gh, "refs/heads/main").is_some(), "github still gets it");
    let cb = of(&events, "mirror_pushed").into_iter().find(|e| e["provider"] == "codeberg").cloned().unwrap();
    assert!(cb["rejected"][0].as_str().unwrap().starts_with("refs/heads/main"), "{cb}");
    let err = of(&events, "error");
    assert_eq!(err.len(), 1, "{events:?}");
    assert_eq!(err[0]["code"], "partial");
    assert!(err[0]["message"].as_str().unwrap().contains(w.cb.to_str().unwrap()), "{err:?}");
}

#[tokio::test]
async fn an_unreachable_forge_fails_alone() {
    let w = world();
    std::fs::remove_dir_all(&w.cb).unwrap();
    let mut p = one(&w);
    p["execute"] = json!(true);
    let events = mirror(&w.config, p).await;
    assert_eq!(of(&events, "mirror_pushed").len(), 1, "{events:?}");
    assert_eq!(of(&events, "mirror_failed").len(), 1, "{events:?}");
    assert_eq!(of(&events, "mirror_summary")[0]["failed"], 1);
    assert_eq!(of(&events, "error")[0]["code"], "partial");
}

#[tokio::test]
async fn status_compares_each_forge_and_writes_nothing() {
    let w = world();
    git(&w.ws.join("widget"), &["push", "-q", w.gh.to_str().unwrap(), "main", "v1"]);
    let mut p = one(&w);
    p["status"] = json!(true);
    let events = mirror(&w.config, p).await;
    let st = of(&events, "mirror_status");
    assert_eq!(st.len(), 2, "{events:?}");
    assert_eq!((st[0]["branches"][0].as_str(), st[0]["tags_missing"].as_u64()), (Some("main: same"), Some(0)));
    assert_eq!((st[1]["branches"][0].as_str(), st[1]["tags_missing"].as_u64()), (Some("main: missing"), Some(1)));
    assert_eq!(sha(&w.cb, "refs/heads/main"), None);
}

#[tokio::test]
async fn the_whole_workspace_plans_in_one_run() {
    let w = world();
    let events = mirror(&w.config, json!({"workspace": "main", "all_branches": true})).await;
    let s = &of(&events, "mirror_summary")[0];
    assert_eq!((s["repos"].as_u64(), s["remotes"].as_u64(), s["planned"].as_u64()), (Some(2), Some(4), Some(4)), "{events:?}");
    assert_eq!(of(&events, "mirror_plan")[0]["refspecs"][0], "refs/heads/*:refs/heads/*");
}

#[tokio::test]
async fn each_forge_authenticates_with_its_own_credential() {
    let w = world();
    // HTTPS remotes on two forges; the plan names the reference each uses.
    std::fs::write(
        w.config.join("orgs/acme.yaml"),
        "name: acme\nforge:\n  provider: github\n  credentials:\n  - key: secrets://github/acme/token\n    type: token\nmirrors:\n- provider: codeberg\n  credentials:\n  - key: secrets://codeberg/acme-mirror/token\n    type: token\nrepos:\n- name: widget\n  remotes:\n  - url: https://github.com/acme/widget.git\n  - url: https://codeberg.org/acme/widget.git\n",
    )
    .unwrap();
    std::fs::write(w.config.join("secrets.yaml"), "github/acme/token: tok-gh\ncodeberg/acme-mirror/token: tok-cb\n").unwrap();
    let events = mirror(&w.config, one(&w)).await;
    let plans = of(&events, "mirror_plan");
    assert_eq!(plans.len(), 2, "{events:?}");
    assert_eq!(plans[0]["auth"], "secrets://github/acme/token");
    assert_eq!(plans[1]["auth"], "secrets://codeberg/acme-mirror/token");
    let text = serde_json::to_string(&events).unwrap();
    assert!(!text.contains("tok-gh") && !text.contains("tok-cb"), "no token in any event");
}

#[tokio::test]
async fn status_and_execute_together_are_refused() {
    let w = world();
    let events = mirror(&w.config, json!({"workspace": "main", "status": true, "execute": true})).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "error");
}

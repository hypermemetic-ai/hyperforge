//! HYPERFORGE-44: `build.release` and `build.release_all` against real git
//! repos and a local bare `origin`. No registry is reachable: no channel is
//! given except where a refusal comes first.

use std::path::{Path, PathBuf};
use std::process::Command;

use futures::StreamExt;
use hyperforge::v5::service::HyperforgeService;
use plexus_core::plexus::{Activation, PlexusStreamItem};
use serde_json::{json, Value};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Rig {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    repo: PathBuf,
    remote: PathBuf,
}

fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let (remote, repo, config) = (root.join("remote.git"), root.join("ws/widget"), root.join("config"));
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(config.join("orgs")).unwrap();
    std::fs::create_dir_all(config.join("workspaces")).unwrap();
    assert!(Command::new("git").args(["init", "-q", "--bare"]).arg(&remote).status().unwrap().success());
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    std::fs::write(repo.join("Cargo.toml"), "[package]\nname = \"widget\"\nversion = \"0.1.0\"\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "seed"]);
    git(&repo, &["push", "-q", "origin", "main"]);
    std::fs::write(config.join("config.yaml"), "provider_map: {}\n").unwrap();
    std::fs::write(
        config.join("orgs/demo.yaml"),
        format!(
            "name: demo\nforge:\n  provider: github\n  credentials: []\nrepos:\n  - name: widget\n    remotes:\n      - url: {}\n",
            remote.display()
        ),
    )
    .unwrap();
    std::fs::write(
        config.join("workspaces/main.yaml"),
        format!("name: main\npath: {}\nrepos:\n  - demo/widget\n", root.join("ws").display()),
    )
    .unwrap();
    Rig { _tmp: tmp, config, repo, remote }
}

async fn call(rig: &Rig, method: &str, params: Value) -> Vec<Value> {
    let service = HyperforgeService::new(rig.config.clone());
    let mut stream = service.call(method, params, None, None).await.unwrap();
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        if let PlexusStreamItem::Data { content, .. } = item {
            events.push(content.into_value());
        }
    }
    events
}

fn release(execute: bool) -> Value {
    json!({"org": "demo", "name": "widget", "bump": "minor", "execute": execute})
}

fn has(events: &[Value], kind: &str) -> bool {
    events.iter().any(|e| e["type"] == kind)
}

fn refused(events: &[Value], code: &str) -> bool {
    events.iter().any(|e| e["type"] == "error" && e["code"] == code)
}

fn manifest(rig: &Rig) -> String {
    std::fs::read_to_string(rig.repo.join("Cargo.toml")).unwrap()
}

#[tokio::test]
async fn without_execute_it_plans_and_writes_nothing() {
    let rig = rig();
    let head = git(&rig.repo, &["rev-parse", "HEAD"]);
    let events = call(&rig, "build.release", json!({"org": "demo", "name": "widget", "bump": "minor"})).await;
    assert!(has(&events, "release_planned"), "{events:?}");
    assert!(!has(&events, "release_created"), "{events:?}");
    assert_eq!(git(&rig.repo, &["rev-parse", "HEAD"]), head);
    assert!(manifest(&rig).contains("0.1.0"));
    assert_eq!(git(&rig.repo, &["tag"]), "");
}

#[tokio::test]
async fn execute_pushes_the_bump_and_an_annotated_tag() {
    let rig = rig();
    let events = call(&rig, "build.release", release(true)).await;
    assert!(has(&events, "release_created"), "{events:?}");
    assert!(!has(&events, "package_published"), "{events:?}");
    assert_eq!(git(&rig.repo, &["cat-file", "-t", "v0.2.0"]), "tag");
    assert_eq!(git(&rig.remote, &["cat-file", "-t", "v0.2.0"]), "tag");
    assert_eq!(git(&rig.remote, &["rev-parse", "main"]), git(&rig.repo, &["rev-parse", "HEAD"]));
}

#[tokio::test]
async fn a_dirty_tree_is_refused() {
    let rig = rig();
    std::fs::write(rig.repo.join("stray.txt"), "x").unwrap();
    let events = call(&rig, "build.release", release(true)).await;
    assert!(refused(&events, "dirty_tree"), "{events:?}");
    assert!(manifest(&rig).contains("0.1.0"));
}

#[tokio::test]
async fn unrelated_unpushed_commits_are_refused_and_stay_unpushed() {
    let rig = rig();
    let pushed = git(&rig.remote, &["rev-parse", "main"]);
    git(&rig.repo, &["commit", "--allow-empty", "-qm", "unrelated"]);
    let events = call(&rig, "build.release", release(true)).await;
    assert!(refused(&events, "unpushed_commits"), "{events:?}");
    assert_eq!(git(&rig.remote, &["rev-parse", "main"]), pushed);
}

#[tokio::test]
async fn a_taken_tag_is_refused() {
    let rig = rig();
    git(&rig.repo, &["tag", "v0.2.0"]);
    let events = call(&rig, "build.release", release(true)).await;
    assert!(refused(&events, "tag_exists"), "{events:?}");
    assert!(manifest(&rig).contains("0.1.0"));
}

#[tokio::test]
async fn a_failed_push_is_reported_not_swallowed() {
    let rig = rig();
    std::fs::write(rig.remote.join("hooks/pre-receive"), "#!/bin/sh\nexit 1\n").unwrap();
    Command::new("chmod").arg("+x").arg(rig.remote.join("hooks/pre-receive")).status().unwrap();
    let events = call(&rig, "build.release", release(true)).await;
    assert!(has(&events, "error"), "{events:?}");
    assert!(!has(&events, "release_created"), "{events:?}");
}

#[tokio::test]
async fn a_missing_token_stops_before_any_write_and_nothing_says_published() {
    let rig = rig();
    let mut params = release(true);
    params["channel"] = json!("crates.io");
    let events = call(&rig, "build.release", params).await;
    assert!(refused(&events, "missing_token"), "{events:?}");
    assert!(!has(&events, "package_published"), "{events:?}");
    assert!(manifest(&rig).contains("0.1.0"));
}

#[tokio::test]
async fn release_all_plans_unless_executed_and_shares_the_gate() {
    let rig = rig();
    let events = call(&rig, "build.release_all", json!({"name": "main"})).await;
    assert!(has(&events, "release_planned"), "{events:?}");
    assert!(manifest(&rig).contains("0.1.0"));

    git(&rig.repo, &["commit", "--allow-empty", "-qm", "unrelated"]);
    let events = call(&rig, "build.release_all", json!({"name": "main", "execute": true})).await;
    assert!(refused(&events, "unpushed_commits"), "{events:?}");
    assert!(manifest(&rig).contains("0.1.0"));

    git(&rig.repo, &["push", "-q", "origin", "main"]);
    let events = call(&rig, "build.release_all", json!({"name": "main", "execute": true})).await;
    assert!(events.iter().any(|e| e["type"] == "release_summary" && e["ok"] == 1 && e["executed"] == true), "{events:?}");
    assert_eq!(git(&rig.repo, &["cat-file", "-t", "v0.1.1"]), "tag");
}

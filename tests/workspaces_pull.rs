//! HYPERFORGE-64: `workspaces pull` and three traps it fell into on
//! 2026-11-06.
//!
//! 1. hyperforge's own `.hyperforge/config.toml`, rewritten by hyperforge,
//!    left members dirty and pull refused them. hyperforge now commits that
//!    file whenever it is tracked and changed: the writer commits it, and a
//!    pull that meets it (left by an older hyperforge) carries it through the
//!    fast-forward and commits it after.
//! 2. A long pull starved the connection: the daemon stopped answering
//!    keepalive pings. Each member's git now runs off the async runtime.
//! 3. Git checkouts under the workspace path that are not members are named
//!    in the pull's summary.
//!
//! Every daemon here listens on an ephemeral port with a temporary config
//! and state directory. Remotes are bare repositories on disk.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use hyperforge::v5::service::HyperforgeService;
use plexus_core::plexus::{Activation, PlexusStreamItem};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

const HF_CONFIG: &str = ".hyperforge/config.toml";

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} in {}: {}", dir.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn identity(dir: &Path) {
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["config", "commit.gpgSign", "false"]);
    git(dir, &["config", "tag.gpgSign", "false"]);
}

fn porcelain(dir: &Path) -> String {
    git(dir, &["status", "--porcelain"])
}

fn subject(dir: &Path, rev: &str) -> String {
    git(dir, &["log", "-1", "--format=%s", rev]).trim().to_string()
}

fn config_toml(org: &str) -> String {
    format!("repo_name = \"a\"\norg = \"{org}\"\nforges = []\n")
}

/// A bare `origin` under `remotes/` and a clone of it at `ws/<name>`, with
/// one commit carrying a tracked `.hyperforge/config.toml`.
fn member(root: &Path, name: &str) -> PathBuf {
    let bare = root.join("remotes").join(format!("{name}.git"));
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "-b", "main"]);
    let seed = root.join("seed").join(name);
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    identity(&seed);
    write(&seed.join("README"), "one\n");
    write(&seed.join(HF_CONFIG), &config_toml("old-org"));
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "-q", "-m", "init"]);
    git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&seed, &["push", "-q", "origin", "main"]);
    let dir = root.join("ws").join(name);
    let out = Command::new("git").args(["clone", "-q"]).arg(&bare).arg(&dir).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    identity(&dir);
    dir
}

/// Another commit on `name`'s origin, made from its seed clone.
fn upstream_commit(root: &Path, name: &str, file: &str, body: &str) {
    let seed = root.join("seed").join(name);
    write(&seed.join(file), body);
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "-q", "-m", &format!("upstream {file}")]);
    git(&seed, &["push", "-q", "origin", "main"]);
}

fn workspace_yaml(config: &Path, ws: &Path, members: &[&str]) {
    let listed: String = members.iter().map(|m| format!("- o/{m}\n")).collect();
    write(&config.join("workspaces/w.yaml"), &format!("name: w\npath: {}\nrepos:\n{listed}", ws.display()));
}

struct Fx {
    root: PathBuf,
    service: HyperforgeService,
    _tmp: tempfile::TempDir,
}

fn fixture(members: &[&str]) -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    for m in members {
        member(&root, m);
    }
    let config = root.join("config");
    workspace_yaml(&config, &root.join("ws"), members);
    let service = HyperforgeService::with_state_dir(config, &root.join("state"));
    Fx { root, service, _tmp: tmp }
}

impl Fx {
    fn ws(&self, name: &str) -> PathBuf {
        self.root.join("ws").join(name)
    }

    async fn call(&self, method: &str, params: Value) -> Vec<Value> {
        let mut stream = self
            .service
            .call(method, params, None, None)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e:?}"));
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            if let PlexusStreamItem::Data { content, .. } = item {
                out.push(content.into_value());
            }
        }
        out
    }
}

fn of_type<'a>(events: &'a [Value], ty: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["type"] == ty).collect()
}

fn results(events: &[Value]) -> Vec<(String, String)> {
    of_type(events, "member_git_result")
        .iter()
        .map(|e| (e["ref"]["name"].as_str().or(e["reference"]["name"].as_str()).unwrap_or("?").to_string(), e["status"].as_str().unwrap().to_string()))
        .collect()
}

fn summary(events: &[Value]) -> &Value {
    let s = of_type(events, "workspace_git_summary");
    assert_eq!(s.len(), 1, "{events:#?}");
    s[0]
}

// ---------------------------------------------------------------------------
// 1. Own-config dirt
// ---------------------------------------------------------------------------

/// Every writer of `.hyperforge/config.toml` goes through
/// `ops::fs::write_hyperforge_config`: a rewrite of the tracked file is
/// committed by itself, leaving the tree clean and any other staged work
/// staged.
#[test]
fn a_rewrite_of_the_tracked_config_is_committed_by_itself() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = member(tmp.path(), "a");
    // Unrelated work in progress: one staged file, one untracked.
    write(&dir.join("staged.txt"), "wip\n");
    git(&dir, &["add", "staged.txt"]);
    write(&dir.join("scratch.txt"), "mine\n");

    let mut cfg = hyperforge::v5::ops::fs::read_hyperforge_config(&dir).unwrap().unwrap();
    cfg.org = hyperforge::v5::config::OrgName::from("hypermemetic-ai");
    hyperforge::v5::ops::fs::write_hyperforge_config(&dir, &cfg, true).unwrap();

    assert_eq!(subject(&dir, "HEAD"), "chore: .hyperforge/config.toml (hyperforge rewrote it)");
    assert_eq!(git(&dir, &["show", "--name-only", "--format=", "HEAD"]).trim(), HF_CONFIG);
    assert_eq!(porcelain(&dir), "A  staged.txt\n?? scratch.txt\n");
    assert!(std::fs::read_to_string(dir.join(HF_CONFIG)).unwrap().contains("hypermemetic-ai"));

    // Writing the same content again commits nothing.
    let head = git(&dir, &["rev-parse", "HEAD"]);
    hyperforge::v5::ops::fs::write_hyperforge_config(&dir, &cfg, true).unwrap();
    assert_eq!(git(&dir, &["rev-parse", "HEAD"]), head);
}

/// A config the repo does not track yet (`repos init` on a fresh checkout)
/// is written and left for the owner to add.
#[test]
fn a_new_untracked_config_is_not_committed() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("fresh");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    identity(&dir);
    write(&dir.join("README"), "x\n");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "init"]);
    let cfg = hyperforge::v5::ops::fs::HyperforgeRepoConfig {
        repo_name: "fresh".into(),
        org: hyperforge::v5::config::OrgName::from("o"),
        forges: Vec::new(),
        default_branch: None,
        visibility: None,
        description: None,
    };
    hyperforge::v5::ops::fs::write_hyperforge_config(&dir, &cfg, false).unwrap();
    assert_eq!(subject(&dir, "HEAD"), "init");
    assert_eq!(porcelain(&dir), "?? .hyperforge/\n");
}

/// The 2026-11-06 failure: a member whose only change is its
/// `.hyperforge/config.toml` (left by an older hyperforge) is pulled, the
/// change rides through the fast-forward, and is then committed.
#[tokio::test]
async fn pull_takes_a_member_whose_only_change_is_its_own_config() {
    let fx = fixture(&["a", "b"]);
    write(&fx.ws("a").join(HF_CONFIG), &config_toml("hypermemetic-ai"));
    upstream_commit(&fx.root, "a", "README", "two\n");

    let ev = fx.call("workspaces.pull", json!({"name": "w"})).await;
    assert_eq!(results(&ev), [("a".into(), "ok".into()), ("b".into(), "ok".into())], "{ev:#?}");
    let a = fx.ws("a");
    assert_eq!(porcelain(&a), "", "{ev:#?}");
    assert_eq!(std::fs::read_to_string(a.join("README")).unwrap(), "two\n");
    assert_eq!(subject(&a, "HEAD"), "chore: .hyperforge/config.toml (hyperforge pull)");
    assert_eq!(subject(&a, "HEAD~1"), "upstream README");
    assert!(std::fs::read_to_string(a.join(HF_CONFIG)).unwrap().contains("hypermemetic-ai"));
}

/// Any other change still blocks, and the config is left as it was.
#[tokio::test]
async fn pull_still_refuses_other_changes() {
    let fx = fixture(&["a"]);
    write(&fx.ws("a").join(HF_CONFIG), &config_toml("hypermemetic-ai"));
    write(&fx.ws("a").join("README"), "local edit\n");
    upstream_commit(&fx.root, "a", "other", "x\n");

    let ev = fx.call("workspaces.pull", json!({"name": "w"})).await;
    assert_eq!(results(&ev), [("a".into(), "errored".into())], "{ev:#?}");
    let msg = of_type(&ev, "member_git_result")[0]["message"].as_str().unwrap().to_string();
    assert!(msg.contains("dirty"), "{msg}");
    assert_eq!(subject(&fx.ws("a"), "HEAD"), "init");
    assert_eq!(porcelain(&fx.ws("a")), " M .hyperforge/config.toml\n M README\n");
}

/// `repos pull` goes through the same check.
#[tokio::test]
async fn repos_pull_takes_its_own_config_too() {
    let fx = fixture(&["a"]);
    let a = fx.ws("a");
    write(&a.join(HF_CONFIG), &config_toml("hypermemetic-ai"));
    upstream_commit(&fx.root, "a", "README", "two\n");
    let ev = fx.call("repos.pull", json!({"path": a.display().to_string()})).await;
    assert_eq!(of_type(&ev, "pull_done").len(), 1, "{ev:#?}");
    assert_eq!(porcelain(&a), "");
    assert_eq!(subject(&a, "HEAD"), "chore: .hyperforge/config.toml (hyperforge pull)");
}

// ---------------------------------------------------------------------------
// 3. Non-members
// ---------------------------------------------------------------------------

/// A clone under the workspace path that is not a member is named in the
/// summary. A plain directory and a linked worktree of a member are not
/// separate repos, so they are not named.
#[tokio::test]
async fn pull_names_checkouts_under_the_path_that_are_not_members() {
    let fx = fixture(&["a"]);
    // Two clones that are not members, a plain dir, a worktree of `a`.
    member(&fx.root, "plexus-rpc");
    member(&fx.root, "stray");
    std::fs::create_dir_all(fx.ws("notes")).unwrap();
    git(&fx.ws("a"), &["worktree", "add", "-q", "-b", "T-1", fx.ws("a-T-1").to_str().unwrap()]);

    let ev = fx.call("workspaces.pull", json!({"name": "w"})).await;
    let s = summary(&ev);
    assert_eq!(s["total"], 1, "{ev:#?}");
    assert_eq!(s["not_members"], json!(["plexus-rpc", "stray"]), "{ev:#?}");
    let note = s["note"].as_str().unwrap();
    assert!(note.contains("2 git checkouts") && note.contains("plexus-rpc, stray") && note.contains("not members"), "{note}");
}

/// With every checkout a member, the summary carries neither field.
#[tokio::test]
async fn a_summary_with_no_strays_has_no_note() {
    let fx = fixture(&["a"]);
    let ev = fx.call("workspaces.pull", json!({"name": "w"})).await;
    let s = summary(&ev);
    assert!(s.get("not_members").is_none() && s.get("note").is_none(), "{s:#?}");
}

// ---------------------------------------------------------------------------
// 2. A long pull keeps the connection alive
// ---------------------------------------------------------------------------

struct Daemon {
    child: Child,
    port: u16,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn daemon_bin() -> PathBuf {
    std::env::var_os("HF_BIN").map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_hyperforge")), PathBuf::from)
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    l.local_addr().unwrap().port()
}

async fn start_daemon(root: &Path) -> Daemon {
    let port = free_port();
    assert_ne!(port, 44104);
    let child = Command::new(daemon_bin())
        .arg("--port")
        .arg(port.to_string())
        .arg("--config-dir")
        .arg(root.join("config"))
        .arg("--state-dir")
        .arg(root.join("state"))
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("home/.config"))
        .env("XDG_STATE_HOME", root.join("home/.state"))
        .spawn()
        .unwrap();
    let d = Daemon { child, port };
    let deadline = Instant::now() + Duration::from_secs(20);
    while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
        assert!(Instant::now() < deadline, "daemon did not listen on {port}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    d
}

/// Pull over members whose remote takes a second to answer each fetch (an
/// `uploadpack` that sleeps), as a pull over many members does over the
/// network. A ping sent while it runs is answered at once, not when the
/// pull ends. Before the fix the pong came after the whole pull: each
/// member's git ran inside the subscription's poll, on a runtime worker,
/// and the connection's task, woken by that subscription, sat in that
/// worker's LIFO slot (which other workers cannot steal) until the poll
/// returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_pull_answers_pings_while_it_runs() {
    const MEMBERS: usize = 5;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let slow = root.join("slow-upload-pack");
    write(&slow, "#!/bin/sh\nsleep 1\nexec git-upload-pack \"$@\"\n");
    assert!(Command::new("chmod").arg("+x").arg(&slow).status().unwrap().success());
    let names: Vec<String> = (0..MEMBERS).map(|i| format!("m{i}")).collect();
    for n in &names {
        let dir = member(root, n);
        git(&dir, &["config", "remote.origin.uploadpack", slow.to_str().unwrap()]);
    }
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    workspace_yaml(&root.join("config"), &root.join("ws"), &refs);

    let d = start_daemon(root).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}", d.port)).await.unwrap();
    let call = json!({
        "jsonrpc": "2.0", "id": 1, "method": "lforge-v5.call",
        "params": {"method": "lforge-v5.hyperforge.workspaces.pull", "params": {"name": "w"}},
    });
    let started = Instant::now();
    ws.send(Message::Text(call.to_string().into())).await.unwrap();

    let mut pinged: Option<Instant> = None;
    let mut pong_after: Option<Duration> = None;
    let mut summary: Option<Value> = None;
    let mut seen = Vec::new();
    let ping_at = tokio::time::sleep(Duration::from_millis(1500));
    tokio::pin!(ping_at);
    let deadline = tokio::time::sleep(Duration::from_secs(60));
    tokio::pin!(deadline);
    while summary.is_none() {
        tokio::select! {
            () = &mut ping_at, if pinged.is_none() => {
                ws.send(Message::Ping(b"hf64".to_vec().into())).await.unwrap();
                pinged = Some(Instant::now());
            }
            () = &mut deadline => panic!("no summary in 60 s; saw {seen:#?}"),
            m = ws.next() => match m.expect("socket closed").unwrap() {
                Message::Pong(p) if &p[..] == b"hf64" => pong_after = pinged.map(|t| t.elapsed()),
                Message::Text(t) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if t.contains("workspace_git_summary") {
                        summary = Some(v.clone());
                    }
                    seen.push(v);
                }
                _ => {}
            },
        }
    }
    let took = started.elapsed();
    let text = summary.unwrap().to_string();
    assert!(text.contains(&format!("\"ok\":{MEMBERS}")), "{text}\n{seen:#?}");
    // The pull itself takes about a second a member.
    assert!(took >= Duration::from_secs(MEMBERS as u64 - 1), "pull took {took:?}");
    let pong = pong_after.expect("no pong while the pull ran");
    assert!(pong < Duration::from_millis(500), "pong took {pong:?} (pull took {took:?})");
}

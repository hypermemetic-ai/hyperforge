//! End-to-end user-story tests for the TRANSPORT epic.
//!
//! See `plans/TRANSPORT/TRANSPORT-3.md` — this file is the executable
//! form of the epic's seven TS-* scenarios, not method-level coverage.
//! Method-level success / failure / idempotency tests live in
//! `integration_test.rs`. The purpose of *these* tests is to answer
//! "did TRANSPORT deliver the workflows it was motivated by?" — so the
//! assertions prove end-states (what `git remote -v` actually returns,
//! what `repo status` actually reports) rather than the exact event
//! discriminators of each intermediate call.
//!
//! Each test spins up its own git repo inside a `tempfile::TempDir` and
//! routes service calls through `DynamicService::route`, the same code path a
//! Synapse CLI call reaches. Config lives under the same tempdir via
//! `HyperforgeService::new_with_config_dir`, so tests are isolated from each
//! other and from the developer's `~/.config/hyperforge`.
//!
//! The recovery scenario (TS-6) explicitly does NOT touch SSH keys,
//! ssh-agent, or `gh` — the TRANSPORT epic's out-of-scope list forbids
//! those. Recovery here means "switch the remote URL shape back to one
//! that works"; credential availability is the caller's concern.

use futures::StreamExt;
use hyperforge::{HyperforgeEvent, HyperforgeService, Transport};
use plexus_core::plexus::{DynamicHub as DynamicService, PlexusStreamItem};
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Build a service rooted at a tempdir config dir. Returns (service, tempdir).
fn test_service() -> (Arc<DynamicService>, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let hyperforge = HyperforgeService::new_with_config_dir(tmp.path().to_path_buf());
    let service = Arc::new(DynamicService::new("testservice").register(hyperforge));
    (service, tmp)
}

/// Drain a routed stream into a vector of `HyperforgeEvent`s.
async fn call(
    service: &DynamicService,
    method: &str,
    params: serde_json::Value,
) -> Vec<HyperforgeEvent> {
    let mut stream = service.route(method, params, None).await.expect("route call");
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        if let PlexusStreamItem::Data { content, .. } = item {
            if let Ok(event) = serde_json::from_value::<HyperforgeEvent>(content.into_value()) {
                events.push(event);
            }
        }
    }
    events
}

/// Create a fresh directory inside `tmp` and run `git init` in it.
fn fresh_repo(tmp: &TempDir, name: &str) -> std::path::PathBuf {
    let repo_path = tmp.path().join(name);
    std::fs::create_dir_all(&repo_path).expect("create repo dir");
    let out = Command::new("git")
        .args(["init"])
        .current_dir(&repo_path)
        .output()
        .expect("git init");
    assert!(out.status.success(), "git init failed: {:?}", out);
    repo_path
}

/// Return the fetch URL of a git remote, or None if the remote doesn't exist.
fn remote_url(repo_path: &std::path::Path, name: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["remote", "-v"])
        .current_dir(repo_path)
        .output()
        .expect("git remote -v");
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 && parts[0] == name && parts[2].contains("fetch") {
            return Some(parts[1].to_string());
        }
    }
    None
}

/// Run `repo status` and return the reported transport for the named forge.
async fn reported_transport(
    service: &DynamicService,
    path: &std::path::Path,
    forge: &str,
) -> Option<Transport> {
    let events = call(
        service,
        "hyperforge.repo.status",
        serde_json::json!({ "path": path.display().to_string() }),
    )
    .await;
    events.into_iter().find_map(|e| match e {
        HyperforgeEvent::RepoTransport { forge: f, transport, .. } if f == forge => transport,
        _ => None,
    })
}

/// Init a repo with the given transport via the service (hyperforge.repo.init).
async fn init_repo(
    service: &DynamicService,
    path: &std::path::Path,
    forges: &str,
    org: &str,
    name: &str,
    transport: Option<&str>,
) {
    let mut params = serde_json::json!({
        "path": path.display().to_string(),
        "forges": forges,
        "org": org,
        "repo_name": name,
        "no_hooks": true,
        "no_ssh_wrapper": true,
    });
    if let Some(t) = transport {
        params["transport"] = serde_json::Value::String(t.to_string());
    }
    call(service, "hyperforge.repo.init", params).await;
}

// ---------------------------------------------------------------------------
// TS-1 — init with SSH (default)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_ts1_init_ssh() {
    let (service, tmp) = test_service();
    let repo = fresh_repo(&tmp, "ts1");

    init_repo(&service, &repo, "github", "alice", "ts1", None).await;

    let url = remote_url(&repo, "origin").expect("origin must exist");
    assert_eq!(url, "git@github.com:alice/ts1.git",
        "omitting transport must give today's SSH default");

    let tx = reported_transport(&service, &repo, "github").await;
    assert_eq!(tx, Some(Transport::Ssh),
        "repo status must report Ssh for the default init");
}

// ---------------------------------------------------------------------------
// TS-2 — init with HTTPS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_ts2_init_https() {
    let (service, tmp) = test_service();
    let repo = fresh_repo(&tmp, "ts2");

    init_repo(&service, &repo, "github", "alice", "ts2", Some("https")).await;

    let url = remote_url(&repo, "origin").expect("origin must exist");
    assert_eq!(url, "https://github.com/alice/ts2.git",
        "--transport https must produce a plain HTTPS URL");
    assert!(!url.contains('@'), "HTTPS URL must not embed credentials");

    let tx = reported_transport(&service, &repo, "github").await;
    assert_eq!(tx, Some(Transport::Https));
}

// ---------------------------------------------------------------------------
// TS-3 — switch SSH -> HTTPS after init
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_ts3_switch_ssh_to_https() {
    let (service, tmp) = test_service();
    let repo = fresh_repo(&tmp, "ts3");

    init_repo(&service, &repo, "github", "alice", "ts3", None).await;
    assert_eq!(
        remote_url(&repo, "origin").unwrap(),
        "git@github.com:alice/ts3.git",
    );

    let events = call(
        &service,
        "hyperforge.repo.set_transport",
        serde_json::json!({
            "path": repo.display().to_string(),
            "transport": "https",
        }),
    )
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            HyperforgeEvent::TransportChanged { transport: Transport::Https, .. }
        )),
        "switch should emit TransportChanged(Https), got {:?}",
        events,
    );

    assert_eq!(
        remote_url(&repo, "origin").unwrap(),
        "https://github.com/alice/ts3.git",
    );
    let tx = reported_transport(&service, &repo, "github").await;
    assert_eq!(tx, Some(Transport::Https),
        "repo status must reflect the switch on the next call");
}

// ---------------------------------------------------------------------------
// TS-4 — switch HTTPS -> SSH
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_ts4_switch_https_to_ssh() {
    let (service, tmp) = test_service();
    let repo = fresh_repo(&tmp, "ts4");

    init_repo(&service, &repo, "github", "alice", "ts4", Some("https")).await;
    assert_eq!(
        remote_url(&repo, "origin").unwrap(),
        "https://github.com/alice/ts4.git",
    );

    call(
        &service,
        "hyperforge.repo.set_transport",
        serde_json::json!({
            "path": repo.display().to_string(),
            "transport": "ssh",
        }),
    )
    .await;

    assert_eq!(
        remote_url(&repo, "origin").unwrap(),
        "git@github.com:alice/ts4.git",
    );
    let tx = reported_transport(&service, &repo, "github").await;
    assert_eq!(tx, Some(Transport::Ssh));
}

// ---------------------------------------------------------------------------
// TS-5 — idempotent no-op
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_ts5_idempotent_noop() {
    let (service, tmp) = test_service();
    let repo = fresh_repo(&tmp, "ts5");

    init_repo(&service, &repo, "github", "alice", "ts5", None).await;

    // First switch changes something (ssh -> https).
    let first = call(
        &service,
        "hyperforge.repo.set_transport",
        serde_json::json!({
            "path": repo.display().to_string(),
            "transport": "https",
        }),
    )
    .await;
    assert!(first.iter().any(|e| matches!(e, HyperforgeEvent::TransportChanged { .. })));

    // Capture mtime of .git/config — the file git remote set-url
    // actually writes. Any subsequent rewrite would bump it.
    let gitcfg = repo.join(".git").join("config");
    let mtime_before = std::fs::metadata(&gitcfg).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Second switch to the same target — must be a no-op.
    let second = call(
        &service,
        "hyperforge.repo.set_transport",
        serde_json::json!({
            "path": repo.display().to_string(),
            "transport": "https",
        }),
    )
    .await;
    assert!(
        second.iter().any(|e| matches!(
            e,
            HyperforgeEvent::TransportUnchanged { transport: Transport::Https, .. }
        )),
        "second switch to same transport must emit TransportUnchanged, got {:?}",
        second,
    );
    assert!(
        !second.iter().any(|e| matches!(e, HyperforgeEvent::TransportChanged { .. })),
        "idempotent no-op must not emit TransportChanged",
    );

    let mtime_after = std::fs::metadata(&gitcfg).unwrap().modified().unwrap();
    assert_eq!(
        mtime_before, mtime_after,
        ".git/config must not be touched by an idempotent no-op switch",
    );
}

// ---------------------------------------------------------------------------
// TS-6 — recovery workflow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_ts6_recovery_workflow() {
    // Simulates the incident that motivated the epic:
    //   1. A repo whose remote was already HTTPS.
    //   2. `repo init` gets run and rewrites the remote to SSH — but
    //      there are no SSH keys, so every subsequent push would fail.
    //   3. The user needs a hyperforge-native recovery: switch back to
    //      HTTPS without leaving the tool. That's exactly what
    //      `repo set_transport --transport https` does.
    let (service, tmp) = test_service();
    let repo = fresh_repo(&tmp, "ts6");

    // Simulate the pre-existing HTTPS remote.
    let out = Command::new("git")
        .args(["remote", "add", "origin", "https://github.com/alice/ts6.git"])
        .current_dir(&repo)
        .output()
        .expect("git remote add");
    assert!(out.status.success());

    // Step 2: `repo init` is run on this repo. With today's default
    // (SSH), init rewrites origin to the SSH form — the exact point of
    // failure described in TRANSPORT-1's motivation.
    init_repo(&service, &repo, "github", "alice", "ts6", None).await;
    assert_eq!(
        remote_url(&repo, "origin").unwrap(),
        "git@github.com:alice/ts6.git",
        "init without --transport must rewrite to SSH (today's default)",
    );

    // Step 3: recovery — switch back to HTTPS via the epic's new method.
    let events = call(
        &service,
        "hyperforge.repo.set_transport",
        serde_json::json!({
            "path": repo.display().to_string(),
            "transport": "https",
        }),
    )
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            HyperforgeEvent::TransportChanged { transport: Transport::Https, .. }
        )),
        "recovery must emit TransportChanged(Https), got {:?}",
        events,
    );

    // Recovery landed: remote is HTTPS again, repo status agrees.
    assert_eq!(
        remote_url(&repo, "origin").unwrap(),
        "https://github.com/alice/ts6.git",
    );
    let tx = reported_transport(&service, &repo, "github").await;
    assert_eq!(tx, Some(Transport::Https),
        "repo status must be clean after recovery");

    // No SSH-side-effects — recovery does not try to configure SSH keys
    // or touch ssh-agent. The event stream carries only transport events.
    let had_ssh_side_effects = events.iter().any(|e| match e {
        HyperforgeEvent::Info { message } => message.to_lowercase().contains("ssh"),
        _ => false,
    });
    assert!(
        !had_ssh_side_effects,
        "recovery must not emit SSH-related events, got {:?}",
        events,
    );
}

// ---------------------------------------------------------------------------
// TS-7 — pre-existing-repo compatibility
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_ts7_pre_existing_repo() {
    // Simulates a repo initialised *before* the TRANSPORT epic: the
    // .hyperforge/config.toml exists but knows nothing about transport,
    // and `git remote -v` reflects whatever the old init wrote. Both
    // `repo status` (read) and `repo set_transport` (write) must just
    // work — no re-init, no migration, no cached-field lookups.
    let (service, tmp) = test_service();
    let repo = fresh_repo(&tmp, "ts7");

    // Hand-craft the legacy .hyperforge/config.toml — exactly what a
    // pre-TRANSPORT init would have written.
    let cfg_dir = repo.join(".hyperforge");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        r#"org = "alice"
repo_name = "ts7"
forges = ["github"]
visibility = "public"
"#,
    )
    .unwrap();

    // Wire origin to the pre-TRANSPORT default (SSH).
    Command::new("git")
        .args(["remote", "add", "origin", "git@github.com:alice/ts7.git"])
        .current_dir(&repo)
        .output()
        .expect("git remote add");

    // `repo status` reports transport correctly — read live, not from
    // any cached field.
    let tx = reported_transport(&service, &repo, "github").await;
    assert_eq!(tx, Some(Transport::Ssh),
        "pre-TRANSPORT repo must report its transport from live git remote");

    // Switching works without re-init.
    let events = call(
        &service,
        "hyperforge.repo.set_transport",
        serde_json::json!({
            "path": repo.display().to_string(),
            "transport": "https",
        }),
    )
    .await;
    assert!(
        events.iter().any(|e| matches!(e, HyperforgeEvent::TransportChanged { .. })),
        "switch must succeed without re-init, got {:?}",
        events,
    );

    assert_eq!(
        remote_url(&repo, "origin").unwrap(),
        "https://github.com/alice/ts7.git",
    );
    let tx2 = reported_transport(&service, &repo, "github").await;
    assert_eq!(tx2, Some(Transport::Https));
}

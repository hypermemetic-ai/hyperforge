//! Integration tests for hyperforge service

use futures::StreamExt;
use plexus_core::plexus::{DynamicHub as DynamicService, PlexusStreamItem};
use hyperforge::{
    HyperforgeEvent, HyperforgeService, OrgAddFailureReason, OrgUpdateFailureReason, OrgUpdateOp,
    Transport, TransportChangeFailureReason,
};
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;

/// Drain a routed stream into a vector of `HyperforgeEvent`s, ignoring any
/// non-`Data` items.
async fn drain_hyperforge_events(service: &DynamicService, method: &str, params: serde_json::Value) -> Vec<HyperforgeEvent> {
    let mut stream = service.route(method, params, None).await.expect("route call");
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        if let PlexusStreamItem::Data { content, .. } = item {
            if let Ok(event) = serde_json::from_value::<HyperforgeEvent>(content) {
                events.push(event);
            }
        }
    }
    events
}

/// Build a service rooted at a tempdir config directory. Returns (service, tempdir).
fn test_service() -> (Arc<DynamicService>, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let hyperforge = HyperforgeService::new_with_config_dir(tmp.path().to_path_buf());
    let service = Arc::new(DynamicService::new("testservice").register(hyperforge));
    (service, tmp)
}

#[tokio::test]
async fn test_hyperforge_as_plugin() {
    // Create hyperforge activation
    let hyperforge = HyperforgeService::new();

    // Register in DynamicService
    let service = Arc::new(DynamicService::new("testservice").register(hyperforge));

    // Call hyperforge.status via DynamicService routing
    let mut stream = service.route("hyperforge.status", serde_json::json!({}), None).await.unwrap();

    let mut found_status = false;
    while let Some(item) = stream.next().await {
        if let plexus_core::plexus::PlexusStreamItem::Data { content, .. } = item {
            if let Ok(HyperforgeEvent::Status { version, description }) =
                serde_json::from_value::<HyperforgeEvent>(content)
            {
                assert_eq!(version, env!("CARGO_PKG_VERSION"));
                assert!(description.contains("FORGE4"));
                found_status = true;
            }
        }
    }

    assert!(found_status, "Should have received status event");
}

#[tokio::test]
async fn test_dynamic_service_lists_hyperforge() {
    // Create hyperforge activation
    let hyperforge = HyperforgeService::new();

    // Register in DynamicService
    let service = DynamicService::new("testservice").register(hyperforge);

    // Check that hyperforge is listed in activations
    let activations = service.list_activations_info();
    let hyperforge_activation = activations
        .iter()
        .find(|a| a.namespace == "hyperforge")
        .expect("hyperforge should be listed in activations");

    assert_eq!(hyperforge_activation.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(hyperforge_activation.description, "Multi-forge repository management");

    // Check that methods are listed
    assert!(hyperforge_activation.methods.contains(&"status".to_string()));
}

// ---------------------------------------------------------------------------
// orgs_add (ORGS-2)
// ---------------------------------------------------------------------------

/// Scrape the list of org names reported by `orgs_list`. Returns only the
/// org names in the order listed (one message line per org, format
/// "  <org> — workspace: ..., forges: [...]").
async fn list_orgs(service: &DynamicService) -> Vec<String> {
    let events = drain_hyperforge_events(service, "hyperforge.orgs_list", serde_json::json!({})).await;
    let mut out = Vec::new();
    for ev in events {
        if let HyperforgeEvent::Info { message } = ev {
            let trimmed = message.trim_start();
            // Skip the terminating summary line "N org(s) configured."
            if trimmed.contains(" org(s) configured") {
                continue;
            }
            if let Some(dash_idx) = trimmed.find(" — ") {
                let name = trimmed[..dash_idx].trim();
                if !name.is_empty() {
                    out.push(name.to_string());
                }
            }
        }
    }
    out
}

#[tokio::test]
async fn test_orgs_add_success_persists_and_shows_in_list() {
    let (service, _tmp) = test_service();

    // dry-run first — must NOT persist
    let preview = drain_hyperforge_events(&service, "hyperforge.orgs_add", serde_json::json!({
        "org": "newtest",
        "ssh": { "github": "/tmp/gh_key" },
        "dry_run": true,
    })).await;

    let preview_ev = preview.iter().find(|e| matches!(e, HyperforgeEvent::OrgAdded { .. }))
        .expect("dry-run should emit OrgAdded");
    match preview_ev {
        HyperforgeEvent::OrgAdded { org, dry_run } => {
            assert_eq!(org, "newtest");
            assert!(*dry_run, "dry_run event should be flagged");
        }
        _ => unreachable!(),
    }

    assert!(!list_orgs(&service).await.contains(&"newtest".to_string()),
        "dry-run must not persist an org");

    // now commit it
    let committed = drain_hyperforge_events(&service, "hyperforge.orgs_add", serde_json::json!({
        "org": "newtest",
        "ssh": { "github": "/tmp/gh_key" },
        "workspace_path": "/tmp/newtest-ws",
    })).await;

    let created = committed.iter().find(|e| matches!(e, HyperforgeEvent::OrgAdded { .. }))
        .expect("commit should emit OrgAdded");
    match created {
        HyperforgeEvent::OrgAdded { org, dry_run } => {
            assert_eq!(org, "newtest");
            assert!(!*dry_run, "non-dry-run OrgAdded should have dry_run=false");
        }
        _ => unreachable!(),
    }

    let orgs = list_orgs(&service).await;
    assert!(orgs.contains(&"newtest".to_string()),
        "orgs_list should include the newly added org, got {:?}", orgs);
}

#[tokio::test]
async fn test_orgs_add_already_exists_is_distinguishable_and_file_untouched() {
    let (service, tmp) = test_service();

    // Create once
    drain_hyperforge_events(&service, "hyperforge.orgs_add", serde_json::json!({
        "org": "dup",
        "ssh": { "github": "/tmp/original" },
    })).await;

    // Capture mtime + content.
    let config_path = tmp.path().join("orgs").join("dup.toml");
    let original_mtime = std::fs::metadata(&config_path).unwrap().modified().unwrap();
    let original_content = std::fs::read_to_string(&config_path).unwrap();

    // Sleep just long enough that any accidental overwrite would be visible.
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Attempt again — must fail with AlreadyExists.
    let retry = drain_hyperforge_events(&service, "hyperforge.orgs_add", serde_json::json!({
        "org": "dup",
        "ssh": { "github": "/tmp/different" },
    })).await;

    let failure = retry.iter().find_map(|e| match e {
        HyperforgeEvent::OrgAddFailed { org, reason, .. } => Some((org.clone(), reason.clone())),
        _ => None,
    }).expect("second orgs_add should emit OrgAddFailed");

    assert_eq!(failure.0, "dup");
    assert_eq!(failure.1, OrgAddFailureReason::AlreadyExists,
        "duplicate must be distinguishable from invalid-name via reason");

    // mtime and content must be unchanged — no silent overwrite.
    let new_mtime = std::fs::metadata(&config_path).unwrap().modified().unwrap();
    let new_content = std::fs::read_to_string(&config_path).unwrap();
    assert_eq!(original_mtime, new_mtime, "refused orgs_add must not touch the existing file");
    assert_eq!(original_content, new_content, "refused orgs_add must leave contents intact");
}

#[tokio::test]
async fn test_orgs_add_invalid_name_is_distinguishable() {
    let (service, _tmp) = test_service();

    // Path traversal
    let traversal = drain_hyperforge_events(&service, "hyperforge.orgs_add", serde_json::json!({
        "org": "../escape",
        "dry_run": true,
    })).await;
    let traversal_failure = traversal.iter().find_map(|e| match e {
        HyperforgeEvent::OrgAddFailed { reason, .. } => Some(reason.clone()),
        _ => None,
    }).expect("path-traversal org name must fail");
    assert_eq!(traversal_failure, OrgAddFailureReason::InvalidName);

    // Slash
    let slash = drain_hyperforge_events(&service, "hyperforge.orgs_add", serde_json::json!({
        "org": "foo/bar",
    })).await;
    let slash_failure = slash.iter().find_map(|e| match e {
        HyperforgeEvent::OrgAddFailed { reason, .. } => Some(reason.clone()),
        _ => None,
    }).expect("slash org name must fail");
    assert_eq!(slash_failure, OrgAddFailureReason::InvalidName);

    // Empty
    let empty = drain_hyperforge_events(&service, "hyperforge.orgs_add", serde_json::json!({
        "org": "",
    })).await;
    let empty_failure = empty.iter().find_map(|e| match e {
        HyperforgeEvent::OrgAddFailed { reason, .. } => Some(reason.clone()),
        _ => None,
    }).expect("empty org name must fail");
    assert_eq!(empty_failure, OrgAddFailureReason::InvalidName);

    // Nothing ever persisted
    assert!(list_orgs(&service).await.is_empty(),
        "no org should have been created by any of the invalid attempts");
}

#[tokio::test]
async fn test_orgs_add_invalid_name_distinct_from_already_exists() {
    // A single invoker must be able to tell the two failure cases apart by
    // inspecting only the event stream — no source-code diffing.
    let (service, _tmp) = test_service();

    // Create a valid org first.
    drain_hyperforge_events(&service, "hyperforge.orgs_add", serde_json::json!({ "org": "already" })).await;

    let already = drain_hyperforge_events(&service, "hyperforge.orgs_add", serde_json::json!({ "org": "already" })).await;
    let invalid = drain_hyperforge_events(&service, "hyperforge.orgs_add", serde_json::json!({ "org": "../bad" })).await;

    let already_reason = already.iter().find_map(|e| match e {
        HyperforgeEvent::OrgAddFailed { reason, .. } => Some(reason.clone()),
        _ => None,
    }).expect("duplicate must emit OrgAddFailed");
    let invalid_reason = invalid.iter().find_map(|e| match e {
        HyperforgeEvent::OrgAddFailed { reason, .. } => Some(reason.clone()),
        _ => None,
    }).expect("invalid-name must emit OrgAddFailed");

    assert_ne!(already_reason, invalid_reason,
        "callers must be able to distinguish already-exists from invalid-name");
    assert_eq!(already_reason, OrgAddFailureReason::AlreadyExists);
    assert_eq!(invalid_reason, OrgAddFailureReason::InvalidName);
}

#[tokio::test]
async fn test_orgs_add_round_trip_preserves_ssh_and_workspace_path() {
    let (service, _tmp) = test_service();

    // Provide values on all three known forges and a workspace_path.
    drain_hyperforge_events(&service, "hyperforge.orgs_add", serde_json::json!({
        "org": "rt",
        "ssh": {
            "github": "/tmp/gh",
            "codeberg": "/tmp/cb",
            "gitlab": "/tmp/gl",
        },
        "workspace_path": "/tmp/rt-ws",
    })).await;

    let list = drain_hyperforge_events(&service, "hyperforge.orgs_list", serde_json::json!({})).await;
    let org_line = list.iter().find_map(|e| match e {
        HyperforgeEvent::Info { message } if message.contains("rt ") => Some(message.clone()),
        _ => None,
    }).expect("orgs_list should report rt");

    assert!(org_line.contains("/tmp/rt-ws"),
        "workspace_path must round-trip through orgs_list, got: {}", org_line);
    // All three forges must appear in the ssh listing
    assert!(org_line.contains("github"), "{}", org_line);
    assert!(org_line.contains("codeberg"), "{}", org_line);
    assert!(org_line.contains("gitlab"), "{}", org_line);
}

#[tokio::test]
async fn test_orgs_add_ssh_omitted_creates_empty_ssh_map() {
    let (service, _tmp) = test_service();

    let events = drain_hyperforge_events(&service, "hyperforge.orgs_add", serde_json::json!({
        "org": "noshh",
    })).await;
    assert!(events.iter().any(|e| matches!(e, HyperforgeEvent::OrgAdded { org, dry_run: false } if org == "noshh")));

    assert!(list_orgs(&service).await.contains(&"noshh".to_string()));
}

// ---------------------------------------------------------------------------
// orgs_update (ORGS-3)
// ---------------------------------------------------------------------------

/// Read the current OrgConfig for `org` from disk, under the service's config dir.
fn read_org_toml(tmp: &TempDir, org: &str) -> toml::Value {
    let p = tmp.path().join("orgs").join(format!("{}.toml", org));
    let raw = std::fs::read_to_string(&p).unwrap_or_default();
    toml::from_str(&raw).unwrap_or(toml::Value::Table(Default::default()))
}

/// Extract the `ssh.<forge>` value for an org, if present.
fn ssh_value(tmp: &TempDir, org: &str, forge: &str) -> Option<String> {
    read_org_toml(tmp, org)
        .get("ssh")
        .and_then(|v| v.get(forge))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Extract the top-level workspace_path of an org, if present.
fn ws_path_value(tmp: &TempDir, org: &str) -> Option<String> {
    read_org_toml(tmp, org)
        .get("workspace_path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Seed an org named `upd` with only github ssh (acceptance-criterion #1 setup).
async fn seed_upd_org(service: &DynamicService) {
    drain_hyperforge_events(service, "hyperforge.orgs_add", serde_json::json!({
        "org": "upd",
        "ssh": { "github": "/tmp/gh_initial" },
    })).await;
}

#[tokio::test]
async fn test_orgs_update_not_found_is_distinguishable() {
    let (service, _tmp) = test_service();

    let events = drain_hyperforge_events(&service, "hyperforge.orgs_update", serde_json::json!({
        "org": "ghost",
        "ssh": { "github": "/tmp/gh" },
    })).await;

    let failure = events.iter().find_map(|e| match e {
        HyperforgeEvent::OrgUpdateFailed { org, reason, .. } => Some((org.clone(), reason.clone())),
        _ => None,
    }).expect("missing-org update should emit OrgUpdateFailed");

    assert_eq!(failure.0, "ghost");
    assert_eq!(failure.1, OrgUpdateFailureReason::NotFound);
}

#[tokio::test]
async fn test_orgs_update_no_fields_is_distinguishable() {
    let (service, _tmp) = test_service();
    seed_upd_org(&service).await;

    let events = drain_hyperforge_events(&service, "hyperforge.orgs_update", serde_json::json!({
        "org": "upd",
    })).await;

    let failure = events.iter().find_map(|e| match e {
        HyperforgeEvent::OrgUpdateFailed { reason, .. } => Some(reason.clone()),
        _ => None,
    }).expect("no-fields update should emit OrgUpdateFailed");
    assert_eq!(failure, OrgUpdateFailureReason::NoFieldsToUpdate,
        "no-fields must be distinguishable from not-found");
}

#[tokio::test]
async fn test_orgs_update_not_found_distinct_from_no_fields() {
    let (service, _tmp) = test_service();
    seed_upd_org(&service).await;

    let not_found = drain_hyperforge_events(&service, "hyperforge.orgs_update", serde_json::json!({
        "org": "ghost",
        "ssh": { "github": "/tmp/gh" },
    })).await;
    let no_fields = drain_hyperforge_events(&service, "hyperforge.orgs_update", serde_json::json!({
        "org": "upd",
    })).await;

    let nf_reason = not_found.iter().find_map(|e| match e {
        HyperforgeEvent::OrgUpdateFailed { reason, .. } => Some(reason.clone()),
        _ => None,
    }).unwrap();
    let nf2_reason = no_fields.iter().find_map(|e| match e {
        HyperforgeEvent::OrgUpdateFailed { reason, .. } => Some(reason.clone()),
        _ => None,
    }).unwrap();

    assert_ne!(nf_reason, nf2_reason,
        "callers must be able to distinguish not-found from no-fields-to-update");
}

#[tokio::test]
async fn test_orgs_update_dry_run_does_not_persist() {
    let (service, tmp) = test_service();
    seed_upd_org(&service).await;

    let before = read_org_toml(&tmp, "upd");

    let events = drain_hyperforge_events(&service, "hyperforge.orgs_update", serde_json::json!({
        "org": "upd",
        "ssh": { "codeberg": "/tmp/cb_new" },
        "dry_run": true,
    })).await;

    let success = events.iter().find_map(|e| match e {
        HyperforgeEvent::OrgUpdated { dry_run, operations, .. } => Some((*dry_run, operations.clone())),
        _ => None,
    }).expect("dry-run should emit OrgUpdated");
    assert!(success.0, "dry_run flag must be true");
    assert_eq!(success.1, vec![OrgUpdateOp::SshMerged]);

    let after = read_org_toml(&tmp, "upd");
    assert_eq!(before, after, "dry-run must not mutate the on-disk TOML");
}

#[tokio::test]
async fn test_orgs_update_default_merge_preserves_other_fields() {
    let (service, tmp) = test_service();
    // Seed with github and codeberg
    drain_hyperforge_events(&service, "hyperforge.orgs_add", serde_json::json!({
        "org": "upd",
        "ssh": { "github": "/tmp/gh_old", "codeberg": "/tmp/cb_old" },
    })).await;

    // Update only github
    let events = drain_hyperforge_events(&service, "hyperforge.orgs_update", serde_json::json!({
        "org": "upd",
        "ssh": { "github": "/tmp/gh_NEW" },
    })).await;
    assert!(events.iter().any(|e| matches!(e, HyperforgeEvent::OrgUpdated {
        operations, dry_run: false, ..
    } if operations == &vec![OrgUpdateOp::SshMerged])));

    // github updated, codeberg untouched
    assert_eq!(ssh_value(&tmp, "upd", "github").as_deref(), Some("/tmp/gh_NEW"));
    assert_eq!(ssh_value(&tmp, "upd", "codeberg").as_deref(), Some("/tmp/cb_old"));
}

#[tokio::test]
async fn test_orgs_update_replace_wipes_untouched_fields() {
    let (service, tmp) = test_service();
    // Seed with both github and codeberg
    drain_hyperforge_events(&service, "hyperforge.orgs_add", serde_json::json!({
        "org": "upd",
        "ssh": { "github": "/tmp/gh", "codeberg": "/tmp/cb" },
    })).await;

    // Replace with empty ssh record (ssh passed, all fields None)
    let events = drain_hyperforge_events(&service, "hyperforge.orgs_update", serde_json::json!({
        "org": "upd",
        "ssh": {},
        "replace": true,
    })).await;

    let ok = events.iter().find_map(|e| match e {
        HyperforgeEvent::OrgUpdated { operations, dry_run: false, .. } => Some(operations.clone()),
        _ => None,
    }).expect("replace should succeed");
    assert!(ok.contains(&OrgUpdateOp::SshReplaced),
        "replace must surface SshReplaced, not SshMerged, got {:?}", ok);

    assert_eq!(ssh_value(&tmp, "upd", "github"), None, "github must be wiped");
    assert_eq!(ssh_value(&tmp, "upd", "codeberg"), None, "codeberg must be wiped");
}

#[tokio::test]
async fn test_orgs_update_workspace_path_three_intents() {
    let (service, tmp) = test_service();
    seed_upd_org(&service).await;

    // Intent: SET
    let set = drain_hyperforge_events(&service, "hyperforge.orgs_update", serde_json::json!({
        "org": "upd",
        "workspace_path": "/tmp/x",
    })).await;
    let set_ops = set.iter().find_map(|e| match e {
        HyperforgeEvent::OrgUpdated { operations, .. } => Some(operations.clone()),
        _ => None,
    }).unwrap();
    assert!(set_ops.contains(&OrgUpdateOp::WorkspacePathSet));
    assert_eq!(ws_path_value(&tmp, "upd").as_deref(), Some("/tmp/x"));

    // Intent: KEEP — ssh mutation with workspace_path omitted.
    let keep = drain_hyperforge_events(&service, "hyperforge.orgs_update", serde_json::json!({
        "org": "upd",
        "ssh": { "github": "/tmp/keep" },
    })).await;
    // Must NOT contain a WorkspacePath* op.
    let keep_ops = keep.iter().find_map(|e| match e {
        HyperforgeEvent::OrgUpdated { operations, .. } => Some(operations.clone()),
        _ => None,
    }).unwrap();
    assert!(!keep_ops.contains(&OrgUpdateOp::WorkspacePathSet));
    assert!(!keep_ops.contains(&OrgUpdateOp::WorkspacePathCleared));
    assert_eq!(ws_path_value(&tmp, "upd").as_deref(), Some("/tmp/x"),
        "omitted workspace_path must preserve the prior value");

    // Intent: CLEAR — empty string.
    let clear = drain_hyperforge_events(&service, "hyperforge.orgs_update", serde_json::json!({
        "org": "upd",
        "workspace_path": "",
    })).await;
    let clear_ops = clear.iter().find_map(|e| match e {
        HyperforgeEvent::OrgUpdated { operations, .. } => Some(operations.clone()),
        _ => None,
    }).unwrap();
    assert!(clear_ops.contains(&OrgUpdateOp::WorkspacePathCleared));
    assert_eq!(ws_path_value(&tmp, "upd"), None,
        "clear intent must leave workspace_path absent from persisted TOML");
}

#[tokio::test]
async fn test_orgs_update_round_trip_through_orgs_list() {
    let (service, _tmp) = test_service();
    seed_upd_org(&service).await;

    // Add codeberg (merge), set a workspace_path, all in one call
    drain_hyperforge_events(&service, "hyperforge.orgs_update", serde_json::json!({
        "org": "upd",
        "ssh": { "codeberg": "/tmp/cb_rt" },
        "workspace_path": "/tmp/upd_ws",
    })).await;

    // orgs_list must surface both forges + the new workspace_path.
    let list = drain_hyperforge_events(&service, "hyperforge.orgs_list", serde_json::json!({})).await;
    let line = list.iter().find_map(|e| match e {
        HyperforgeEvent::Info { message } if message.contains("upd ") => Some(message.clone()),
        _ => None,
    }).expect("orgs_list must include upd");

    assert!(line.contains("/tmp/upd_ws"), "workspace_path missing from list: {}", line);
    assert!(line.contains("github"), "github forge missing from list: {}", line);
    assert!(line.contains("codeberg"), "codeberg forge missing from list: {}", line);
}

// ---------------------------------------------------------------------------
// repo init / repo set_transport (TRANSPORT-2)
// ---------------------------------------------------------------------------

/// Build a scratch git repo dir inside `tmp` and return its path. The dir
/// is initialised as a git repo so `repo init` has something to wire remotes
/// against.
fn fresh_repo_dir(tmp: &TempDir, name: &str) -> std::path::PathBuf {
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

/// Return the fetch URL of a named git remote inside `repo_path`, or None
/// if the remote doesn't exist. Uses raw `git remote -v` output so we're
/// reading the same source of truth TRANSPORT-2 asserts on.
fn remote_fetch_url(repo_path: &std::path::Path, name: &str) -> Option<String> {
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

#[tokio::test]
async fn test_repo_init_default_transport_is_ssh() {
    let (service, tmp) = test_service();
    let repo_path = fresh_repo_dir(&tmp, "default-ssh");

    let events = drain_hyperforge_events(&service, "hyperforge.repo.init", serde_json::json!({
        "path": repo_path.display().to_string(),
        "forges": "github",
        "org": "alice",
        "repo_name": "default-ssh",
        "no_hooks": true,
        "no_ssh_wrapper": true,
    })).await;
    assert!(
        !events.iter().any(|e| matches!(e, HyperforgeEvent::Error { .. })),
        "init with no transport flag should not error, got {:?}",
        events,
    );

    let url = remote_fetch_url(&repo_path, "origin").expect("origin should exist");
    assert_eq!(url, "git@github.com:alice/default-ssh.git",
        "omitting transport must preserve today's SSH default");
}

#[tokio::test]
async fn test_repo_init_transport_ssh_produces_ssh_url() {
    let (service, tmp) = test_service();
    let repo_path = fresh_repo_dir(&tmp, "explicit-ssh");

    drain_hyperforge_events(&service, "hyperforge.repo.init", serde_json::json!({
        "path": repo_path.display().to_string(),
        "forges": "github",
        "org": "alice",
        "repo_name": "explicit-ssh",
        "transport": "ssh",
        "no_hooks": true,
        "no_ssh_wrapper": true,
    })).await;

    let url = remote_fetch_url(&repo_path, "origin").expect("origin should exist");
    assert_eq!(url, "git@github.com:alice/explicit-ssh.git");
}

#[tokio::test]
async fn test_repo_init_transport_https_produces_https_url() {
    let (service, tmp) = test_service();
    let repo_path = fresh_repo_dir(&tmp, "explicit-https");

    drain_hyperforge_events(&service, "hyperforge.repo.init", serde_json::json!({
        "path": repo_path.display().to_string(),
        "forges": "github",
        "org": "alice",
        "repo_name": "explicit-https",
        "transport": "https",
        "no_hooks": true,
        "no_ssh_wrapper": true,
    })).await;

    let url = remote_fetch_url(&repo_path, "origin").expect("origin should exist");
    assert_eq!(url, "https://github.com/alice/explicit-https.git",
        "transport=https must write the plain HTTPS URL (no credentials, no auth prefix)");
    assert!(!url.contains('@'),
        "HTTPS URL must not embed credentials via user@ prefix");
}

#[tokio::test]
async fn test_repo_set_transport_ssh_to_https() {
    let (service, tmp) = test_service();
    let repo_path = fresh_repo_dir(&tmp, "switch");

    // Initialize as SSH (today's default).
    drain_hyperforge_events(&service, "hyperforge.repo.init", serde_json::json!({
        "path": repo_path.display().to_string(),
        "forges": "github",
        "org": "alice",
        "repo_name": "switch",
        "no_hooks": true,
        "no_ssh_wrapper": true,
    })).await;
    assert_eq!(
        remote_fetch_url(&repo_path, "origin").unwrap(),
        "git@github.com:alice/switch.git",
    );

    // Switch to HTTPS.
    let events = drain_hyperforge_events(&service, "hyperforge.repo.set_transport", serde_json::json!({
        "path": repo_path.display().to_string(),
        "transport": "https",
    })).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            HyperforgeEvent::TransportChanged { transport: Transport::Https, remotes_changed, .. }
            if remotes_changed.contains(&"origin".to_string())
        )),
        "switching should emit TransportChanged with origin in the list, got {:?}",
        events,
    );

    // Origin is now HTTPS, and there is no stray SSH URL hanging around.
    let url = remote_fetch_url(&repo_path, "origin").unwrap();
    assert_eq!(url, "https://github.com/alice/switch.git");
    assert!(!url.contains("git@"));
}

#[tokio::test]
async fn test_repo_set_transport_idempotent_second_call_is_noop() {
    let (service, tmp) = test_service();
    let repo_path = fresh_repo_dir(&tmp, "idem");

    drain_hyperforge_events(&service, "hyperforge.repo.init", serde_json::json!({
        "path": repo_path.display().to_string(),
        "forges": "github",
        "org": "alice",
        "repo_name": "idem",
        "no_hooks": true,
        "no_ssh_wrapper": true,
    })).await;

    // First switch to https — actually changes something.
    let first = drain_hyperforge_events(&service, "hyperforge.repo.set_transport", serde_json::json!({
        "path": repo_path.display().to_string(),
        "transport": "https",
    })).await;
    assert!(
        first.iter().any(|e| matches!(e, HyperforgeEvent::TransportChanged { .. })),
        "first switch should emit TransportChanged, got {:?}",
        first,
    );

    // Capture mtime of origin's URL storage (via config file under .git/config)
    let git_config_path = repo_path.join(".git").join("config");
    let first_mtime = std::fs::metadata(&git_config_path).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Second switch with the same target — must be idempotent.
    let second = drain_hyperforge_events(&service, "hyperforge.repo.set_transport", serde_json::json!({
        "path": repo_path.display().to_string(),
        "transport": "https",
    })).await;
    assert!(
        second.iter().any(|e| matches!(
            e,
            HyperforgeEvent::TransportUnchanged { transport: Transport::Https, .. }
        )),
        "second switch to the same transport must emit TransportUnchanged, got {:?}",
        second,
    );
    assert!(
        !second.iter().any(|e| matches!(
            e,
            HyperforgeEvent::TransportChanged { .. } | HyperforgeEvent::Error { .. }
                | HyperforgeEvent::TransportChangeFailed { .. }
        )),
        "idempotent no-op must not emit any change/failure events, got {:?}",
        second,
    );

    // Stronger idempotency signal: .git/config mtime must not change.
    let second_mtime = std::fs::metadata(&git_config_path).unwrap().modified().unwrap();
    assert_eq!(first_mtime, second_mtime,
        "idempotent set_transport must not invoke `git remote set-url`, which rewrites .git/config");
}

#[tokio::test]
async fn test_repo_set_transport_https_to_ssh() {
    let (service, tmp) = test_service();
    let repo_path = fresh_repo_dir(&tmp, "back-to-ssh");

    // Initialize as HTTPS.
    drain_hyperforge_events(&service, "hyperforge.repo.init", serde_json::json!({
        "path": repo_path.display().to_string(),
        "forges": "github",
        "org": "alice",
        "repo_name": "back-to-ssh",
        "transport": "https",
        "no_hooks": true,
        "no_ssh_wrapper": true,
    })).await;
    assert_eq!(
        remote_fetch_url(&repo_path, "origin").unwrap(),
        "https://github.com/alice/back-to-ssh.git",
    );

    // Switch to SSH.
    drain_hyperforge_events(&service, "hyperforge.repo.set_transport", serde_json::json!({
        "path": repo_path.display().to_string(),
        "transport": "ssh",
    })).await;

    assert_eq!(
        remote_fetch_url(&repo_path, "origin").unwrap(),
        "git@github.com:alice/back-to-ssh.git",
    );
}

#[tokio::test]
async fn test_repo_set_transport_on_not_initialized_fails_distinguishably() {
    let (service, tmp) = test_service();
    // A bare git repo with no .hyperforge/config.toml.
    let repo_path = fresh_repo_dir(&tmp, "bare");

    let events = drain_hyperforge_events(&service, "hyperforge.repo.set_transport", serde_json::json!({
        "path": repo_path.display().to_string(),
        "transport": "https",
    })).await;

    let failure = events.iter().find_map(|e| match e {
        HyperforgeEvent::TransportChangeFailed { reason, .. } => Some(reason.clone()),
        _ => None,
    }).expect("set_transport on unregistered repo must fail");
    assert_eq!(failure, TransportChangeFailureReason::NotInitialized);

    // No remote was created as a side effect.
    assert!(remote_fetch_url(&repo_path, "origin").is_none());
}

#[tokio::test]
async fn test_repo_set_transport_on_missing_path_fails_distinguishably() {
    let (service, tmp) = test_service();
    let missing = tmp.path().join("does-not-exist");

    let events = drain_hyperforge_events(&service, "hyperforge.repo.set_transport", serde_json::json!({
        "path": missing.display().to_string(),
        "transport": "https",
    })).await;

    let failure = events.iter().find_map(|e| match e {
        HyperforgeEvent::TransportChangeFailed { reason, .. } => Some(reason.clone()),
        _ => None,
    }).expect("set_transport on missing path must fail");
    assert_eq!(failure, TransportChangeFailureReason::PathNotFound);
}

#[tokio::test]
async fn test_repo_set_transport_unsupported_value_rejected_at_parse() {
    // Synapse's CLI parser would refuse unknown transports before they
    // reach the service, because Transport is a closed-set enum (serde
    // rename_all = "lowercase"). From Rust we exercise that by feeding
    // an unknown value through DynamicService::route — the router should
    // refuse to deserialize it into the typed parameter.
    let (service, tmp) = test_service();
    let repo_path = fresh_repo_dir(&tmp, "parse-reject");
    drain_hyperforge_events(&service, "hyperforge.repo.init", serde_json::json!({
        "path": repo_path.display().to_string(),
        "forges": "github",
        "org": "alice",
        "repo_name": "parse-reject",
        "no_hooks": true,
        "no_ssh_wrapper": true,
    })).await;

    // Baseline: origin is the standard SSH URL.
    let pre = remote_fetch_url(&repo_path, "origin").unwrap();

    let result = service
        .route(
            "hyperforge.repo.set_transport",
            serde_json::json!({
                "path": repo_path.display().to_string(),
                "transport": "ftp",
            }),
            None,
        )
        .await;

    // The route call MUST fail; alternatively, if it succeeds it MUST
    // NOT produce any TransportChanged event and the remote must be
    // untouched. Either shape proves the unknown variant was rejected
    // at a layer above the actual rewrite.
    match result {
        Err(_) => {
            // Parse rejection — exactly the expected path.
        }
        Ok(mut stream) => {
            let mut events = Vec::new();
            while let Some(item) = stream.next().await {
                if let PlexusStreamItem::Data { content, .. } = item {
                    if let Ok(ev) = serde_json::from_value::<HyperforgeEvent>(content) {
                        events.push(ev);
                    }
                }
            }
            assert!(
                !events.iter().any(|e| matches!(e, HyperforgeEvent::TransportChanged { .. })),
                "unknown transport must not reach the change path, got {:?}",
                events,
            );
        }
    }

    // Post-state: the remote is unchanged.
    let post = remote_fetch_url(&repo_path, "origin").unwrap();
    assert_eq!(pre, post, "unsupported transport value must not mutate remotes");
}

#[tokio::test]
async fn test_repo_status_reports_transport_via_event_stream() {
    let (service, tmp) = test_service();
    let repo_path = fresh_repo_dir(&tmp, "status-tx");

    // Initialise as HTTPS so the assertion below is specific.
    drain_hyperforge_events(&service, "hyperforge.repo.init", serde_json::json!({
        "path": repo_path.display().to_string(),
        "forges": "github",
        "org": "alice",
        "repo_name": "status-tx",
        "transport": "https",
        "no_hooks": true,
        "no_ssh_wrapper": true,
    })).await;

    let events = drain_hyperforge_events(&service, "hyperforge.repo.status", serde_json::json!({
        "path": repo_path.display().to_string(),
    })).await;

    let reported = events.iter().find_map(|e| match e {
        HyperforgeEvent::RepoTransport { forge, transport, .. } if forge == "github" => {
            Some(transport.clone())
        }
        _ => None,
    }).expect("repo status must emit a RepoTransport event for each configured forge");
    assert_eq!(reported, Some(Transport::Https));

    // Flip transport via the new method and re-read status.
    drain_hyperforge_events(&service, "hyperforge.repo.set_transport", serde_json::json!({
        "path": repo_path.display().to_string(),
        "transport": "ssh",
    })).await;

    let events2 = drain_hyperforge_events(&service, "hyperforge.repo.status", serde_json::json!({
        "path": repo_path.display().to_string(),
    })).await;
    let reported2 = events2.iter().find_map(|e| match e {
        HyperforgeEvent::RepoTransport { forge, transport, .. } if forge == "github" => {
            Some(transport.clone())
        }
        _ => None,
    }).expect("repo status must re-report transport after a switch");
    assert_eq!(reported2, Some(Transport::Ssh),
        "switching transport must be reflected in repo status on the next call");
}

#[tokio::test]
async fn test_repo_status_transport_on_pre_existing_repo() {
    // Simulates a repo that predates the TRANSPORT epic: a
    // `.hyperforge/config.toml` exists but was written without any
    // awareness of transport, and `git remote -v` was set up in a
    // previous era. `repo status` must still report the transport
    // correctly — read from live `git remote` state, not a cached field.
    let (service, tmp) = test_service();
    let repo_path = fresh_repo_dir(&tmp, "legacy");

    // Hand-craft the .hyperforge/config.toml just like an older init
    // would have written.
    let cfg_dir = repo_path.join(".hyperforge");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        r#"org = "alice"
repo_name = "legacy"
forges = ["github"]
visibility = "public"
"#,
    ).unwrap();

    // Manually wire origin to an SSH URL (the pre-TRANSPORT default).
    Command::new("git")
        .args(["remote", "add", "origin", "git@github.com:alice/legacy.git"])
        .current_dir(&repo_path)
        .output()
        .expect("git remote add");

    let events = drain_hyperforge_events(&service, "hyperforge.repo.status", serde_json::json!({
        "path": repo_path.display().to_string(),
    })).await;

    let reported = events.iter().find_map(|e| match e {
        HyperforgeEvent::RepoTransport { forge, transport, .. } if forge == "github" => {
            Some(transport.clone())
        }
        _ => None,
    }).expect("repo status on a pre-TRANSPORT repo must still surface RepoTransport");
    assert_eq!(reported, Some(Transport::Ssh));

    // And switching works without requiring re-init.
    drain_hyperforge_events(&service, "hyperforge.repo.set_transport", serde_json::json!({
        "path": repo_path.display().to_string(),
        "transport": "https",
    })).await;
    assert_eq!(
        remote_fetch_url(&repo_path, "origin").unwrap(),
        "https://github.com/alice/legacy.git",
    );
}

#[tokio::test]
async fn test_repo_set_transport_multi_forge_updates_every_remote() {
    // An init with multiple forges creates `origin` for the first and a
    // named remote per additional forge. A single set_transport call
    // must bring *every* managed remote to the requested transport.
    let (service, tmp) = test_service();
    let repo_path = fresh_repo_dir(&tmp, "multi");

    drain_hyperforge_events(&service, "hyperforge.repo.init", serde_json::json!({
        "path": repo_path.display().to_string(),
        "forges": "github,codeberg",
        "org": "alice",
        "repo_name": "multi",
        "no_hooks": true,
        "no_ssh_wrapper": true,
    })).await;

    assert!(remote_fetch_url(&repo_path, "origin").unwrap().starts_with("git@"));
    assert!(remote_fetch_url(&repo_path, "codeberg").unwrap().starts_with("git@"));

    let events = drain_hyperforge_events(&service, "hyperforge.repo.set_transport", serde_json::json!({
        "path": repo_path.display().to_string(),
        "transport": "https",
    })).await;
    let changed = events.iter().find_map(|e| match e {
        HyperforgeEvent::TransportChanged { remotes_changed, .. } => Some(remotes_changed.clone()),
        _ => None,
    }).expect("multi-forge switch should emit TransportChanged");
    assert!(changed.contains(&"origin".to_string()));
    assert!(changed.contains(&"codeberg".to_string()));

    assert!(remote_fetch_url(&repo_path, "origin").unwrap().starts_with("https://"));
    assert!(remote_fetch_url(&repo_path, "codeberg").unwrap().starts_with("https://"));
}

//! Tests for the `WorkspaceService` → `BuildService` split.
//!
//! Covers:
//! - Schema: method lists for both services are correct and non-overlapping
//! - Routing: `DynamicService` routes to build child service via dotted paths
//! - Dispatch: build methods actually execute and return typed events
//! - Utils: shared helpers (`glob_match`, `dry_prefix`)

use futures::StreamExt;
use hyperforge::services::build::BuildService;
use hyperforge::services::workspace::WorkspaceService;
use hyperforge::services::HyperforgeState;
use hyperforge::{HyperforgeEvent, HyperforgeService};
use plexus_core::plexus::{Activation, ChildRouter, DynamicHub as DynamicService, PlexusStreamItem};
use std::collections::HashSet;
use std::sync::Arc;

// ============================================================================
// Helpers
// ============================================================================

/// Collect all `HyperforgeEvent` items from a `PlexusStream`.
async fn collect_events(
    mut stream: plexus_core::plexus::PlexusStream,
) -> Vec<HyperforgeEvent> {
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

/// Set up a temp workspace with fake Cargo repos for testing.
/// Returns the temp dir (cleaned up on drop) and its path.
fn make_test_workspace() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Create two fake Cargo repos with .git dirs
    for (name, version, deps) in &[
        ("alpha", "0.1.0", vec![]),
        ("beta", "0.2.0", vec![("alpha", "0.1.0")]),
    ] {
        let repo_dir = root.join(name);
        std::fs::create_dir_all(repo_dir.join(".git")).unwrap();
        std::fs::create_dir_all(repo_dir.join(".hyperforge")).unwrap();

        // Cargo.toml
        let mut cargo_toml = format!(
            r#"[package]
name = "{name}"
version = "{version}"
edition = "2021"
"#
        );
        if !deps.is_empty() {
            cargo_toml.push_str("\n[dependencies]\n");
            for (dep_name, dep_ver) in deps {
                cargo_toml.push_str(&format!("{dep_name} = \"{dep_ver}\"\n"));
            }
        }
        std::fs::write(repo_dir.join("Cargo.toml"), cargo_toml).unwrap();

        // .hyperforge/config.toml
        let config = format!(
            r#"org = "testorg"
forges = ["github"]
repo_name = "{name}"
"#
        );
        std::fs::write(repo_dir.join(".hyperforge/config.toml"), config).unwrap();
    }

    // Create an unconfigured repo (git but no .hyperforge)
    let unconfigured = root.join("gamma");
    std::fs::create_dir_all(unconfigured.join(".git")).unwrap();

    tmp
}

// ============================================================================
// Schema tests
// ============================================================================

#[test]
fn build_service_has_correct_methods() {
    let service = BuildService::new();
    let methods: HashSet<&str> = service.methods().into_iter().collect();

    let expected: HashSet<&str> = [
        "unify",
        "analyze",
        "detect_name_mismatches",
        "package_diff",
        "publish",
        "bump",
        "exec",
        "validate",
        "run",
        "init_configs",
        "gitignore_sync",
        "large_files",
        "repo_sizes",
        "dirty",
        "loc",
        "release",
        "release_all",
        "binstall_init",
        "brew_formula",
        "dist_show",
        "dist_init",
        "schema",
    ]
    .into_iter()
    .collect();

    assert_eq!(methods, expected, "BuildService methods mismatch");
}

#[test]
fn workspace_service_has_correct_methods() {
    let service = WorkspaceService::new(HyperforgeState::new());
    let methods: HashSet<&str> = service.methods().into_iter().collect();

    let expected: HashSet<&str> = [
        "discover",
        "init",
        "check",
        "push_all",
        "diff",
        "sync",
        "set_default_branch",
        "check_default_branch",
        "verify",
        "clone",
        "move_repos",
        "schema",
    ]
    .into_iter()
    .collect();

    assert_eq!(methods, expected, "WorkspaceService methods mismatch");
}

#[test]
fn no_method_overlap_between_services() {
    let build = BuildService::new();
    let workspace = WorkspaceService::new(HyperforgeState::new());

    let build_methods: HashSet<&str> = build.methods().into_iter().collect();
    let workspace_methods: HashSet<&str> = workspace.methods().into_iter().collect();

    let overlap: HashSet<&&str> = build_methods
        .intersection(&workspace_methods)
        .collect();

    // "schema" is expected on both — it's auto-generated
    let non_schema_overlap: Vec<&&&str> = overlap.iter().filter(|m| ***m != "schema").collect();

    assert!(
        non_schema_overlap.is_empty(),
        "Unexpected method overlap between BuildService and WorkspaceService: {non_schema_overlap:?}"
    );
}

#[test]
fn build_service_schema_metadata() {
    let service = BuildService::new();
    let schema = service.plugin_schema();

    assert_eq!(schema.namespace, "build");
    assert!(
        schema.description.contains("Development tools"),
        "description should mention development tools, got: {}",
        schema.description
    );
}

#[test]
fn root_service_lists_build_as_child() {
    let service = HyperforgeService::new();
    let children = service.plugin_children();

    let child_namespaces: Vec<&str> = children.iter().map(|c| c.namespace.as_str()).collect();

    assert!(
        child_namespaces.contains(&"build"),
        "root service should list 'build' as child, got: {child_namespaces:?}"
    );
    assert!(
        child_namespaces.contains(&"repo"),
        "root service should list 'repo' as child"
    );
    assert!(
        child_namespaces.contains(&"workspace"),
        "root service should list 'workspace' as child"
    );
    assert_eq!(child_namespaces.len(), 3);
}

// ============================================================================
// Child router tests
// ============================================================================

#[tokio::test]
async fn root_service_routes_to_build_child() {
    let service = HyperforgeService::new();

    let child = service.get_child("build").await;
    assert!(child.is_some(), "get_child('build') should return Some");
    assert_eq!(child.unwrap().router_namespace(), "build");
}

#[tokio::test]
async fn root_service_routes_to_workspace_child() {
    let service = HyperforgeService::new();

    let child = service.get_child("workspace").await;
    assert!(child.is_some(), "get_child('workspace') should return Some");
    assert_eq!(child.unwrap().router_namespace(), "workspace");
}

#[tokio::test]
async fn root_service_rejects_unknown_child() {
    let service = HyperforgeService::new();
    assert!(service.get_child("bogus").await.is_none());
}

// ============================================================================
// Dispatch tests via DynamicService routing
// ============================================================================

#[tokio::test]
async fn route_build_analyze_on_test_workspace() {
    let tmp = make_test_workspace();
    let service = Arc::new(DynamicService::new("test").register(HyperforgeService::new()));

    let stream = service
        .route(
            "hyperforge.build.analyze",
            serde_json::json!({ "path": tmp.path().to_str().unwrap() }),
            None,
        )
        .await
        .expect("route should succeed");

    let events = collect_events(stream).await;

    // Should have at least one info event about the workspace
    assert!(
        !events.is_empty(),
        "analyze should produce events"
    );

    // Should find our dep_mismatch or info about tiers
    let has_info = events.iter().any(|e| matches!(e, HyperforgeEvent::Info { .. }));
    assert!(has_info, "analyze should produce Info events");
}

#[tokio::test]
async fn route_build_detect_name_mismatches() {
    let tmp = make_test_workspace();
    let service = Arc::new(DynamicService::new("test").register(HyperforgeService::new()));

    let stream = service
        .route(
            "hyperforge.build.detect_name_mismatches",
            serde_json::json!({ "path": tmp.path().to_str().unwrap() }),
            None,
        )
        .await
        .expect("route should succeed");

    let events = collect_events(stream).await;

    // Should report the unconfigured "gamma" repo
    let unconfigured_msgs: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            HyperforgeEvent::Info { message } if message.contains("UNCONFIGURED") => {
                Some(message.clone())
            }
            _ => None,
        })
        .collect();

    assert!(
        unconfigured_msgs.iter().any(|m| m.contains("gamma")),
        "should report gamma as unconfigured, got: {unconfigured_msgs:?}"
    );
}

#[tokio::test]
async fn route_build_exec_echo() {
    let tmp = make_test_workspace();
    let service = Arc::new(DynamicService::new("test").register(HyperforgeService::new()));

    let stream = service
        .route(
            "hyperforge.build.exec",
            serde_json::json!({
                "path": tmp.path().to_str().unwrap(),
                "command": "echo hello",
                "include": ["alpha"],
            }),
            None,
        )
        .await
        .expect("route should succeed");

    let events = collect_events(stream).await;

    let exec_results: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            HyperforgeEvent::ExecResult {
                repo_name,
                exit_code,
                stdout,
                ..
            } => Some((repo_name.clone(), *exit_code, stdout.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(exec_results.len(), 1, "should have exactly 1 exec result");
    assert_eq!(exec_results[0].0, "alpha");
    assert_eq!(exec_results[0].1, 0);
    assert!(exec_results[0].2.trim() == "hello");
}

#[tokio::test]
async fn route_build_exec_filter_excludes() {
    let tmp = make_test_workspace();
    let service = Arc::new(DynamicService::new("test").register(HyperforgeService::new()));

    let stream = service
        .route(
            "hyperforge.build.exec",
            serde_json::json!({
                "path": tmp.path().to_str().unwrap(),
                "command": "echo hi",
                "include": ["nonexistent*"],
            }),
            None,
        )
        .await
        .expect("route should succeed");

    let events = collect_events(stream).await;

    // Should have "No repos matched filter" info, no ExecResults
    let exec_count = events
        .iter()
        .filter(|e| matches!(e, HyperforgeEvent::ExecResult { .. }))
        .count();

    assert_eq!(exec_count, 0, "filter should exclude all repos");

    let has_no_match = events.iter().any(|e| match e {
        HyperforgeEvent::Info { message } => message.contains("No repos matched"),
        _ => false,
    });
    assert!(has_no_match, "should report no repos matched filter");
}

#[tokio::test]
async fn route_build_unify_dry_run() {
    let tmp = make_test_workspace();
    let service = Arc::new(DynamicService::new("test").register(HyperforgeService::new()));

    let stream = service
        .route(
            "hyperforge.build.unify",
            serde_json::json!({
                "path": tmp.path().to_str().unwrap(),
                "dry_run": true,
            }),
            None,
        )
        .await
        .expect("route should succeed");

    let events = collect_events(stream).await;

    // Should find Rust crates
    let has_rust_info = events.iter().any(|e| match e {
        HyperforgeEvent::Info { message } => message.contains("Rust crates"),
        _ => false,
    });
    assert!(has_rust_info, "unify should find Rust crates in test workspace");

    // Should have a UnifyResult for cargo config
    let has_unify_result = events
        .iter()
        .any(|e| matches!(e, HyperforgeEvent::UnifyResult { .. }));
    assert!(
        has_unify_result,
        "unify should produce UnifyResult events"
    );

    // Dry run should NOT write .cargo/config.toml
    assert!(
        !tmp.path().join(".cargo/config.toml").exists(),
        "dry_run should not write files"
    );
}

#[tokio::test]
async fn route_build_bump_dry_run() {
    let tmp = make_test_workspace();
    let service = Arc::new(DynamicService::new("test").register(HyperforgeService::new()));

    let stream = service
        .route(
            "hyperforge.build.bump",
            serde_json::json!({
                "path": tmp.path().to_str().unwrap(),
                "bump": "patch",
                "dry_run": true,
                "include": ["alpha"],
            }),
            None,
        )
        .await
        .expect("route should succeed");

    let events = collect_events(stream).await;

    // Should produce a PublishStep with AutoBump for alpha
    let bumps: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            HyperforgeEvent::PublishStep {
                package_name,
                version,
                ..
            } => Some((package_name.clone(), version.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(bumps.len(), 1, "should bump exactly 1 package");
    assert_eq!(bumps[0].0, "alpha");
    assert_eq!(bumps[0].1, "0.1.1", "patch bump 0.1.0 -> 0.1.1");

    // Dry run: Cargo.toml should still say 0.1.0
    let cargo = std::fs::read_to_string(tmp.path().join("alpha/Cargo.toml")).unwrap();
    assert!(
        cargo.contains("0.1.0"),
        "dry_run should not modify Cargo.toml"
    );
}

// ============================================================================
// Workspace methods still work
// ============================================================================

#[tokio::test]
async fn route_workspace_discover_still_works() {
    let tmp = make_test_workspace();
    let service = Arc::new(DynamicService::new("test").register(HyperforgeService::new()));

    let stream = service
        .route(
            "hyperforge.workspace.discover",
            serde_json::json!({ "path": tmp.path().to_str().unwrap() }),
            None,
        )
        .await
        .expect("route should succeed");

    let events = collect_events(stream).await;

    // Should find alpha and beta
    let info_msgs: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            HyperforgeEvent::Info { message } => Some(message.clone()),
            _ => None,
        })
        .collect();

    assert!(
        info_msgs.iter().any(|m| m.contains("alpha")),
        "discover should find alpha"
    );
    assert!(
        info_msgs.iter().any(|m| m.contains("beta")),
        "discover should find beta"
    );

    // Should have a WorkspaceSummary
    let has_summary = events
        .iter()
        .any(|e| matches!(e, HyperforgeEvent::WorkspaceSummary { .. }));
    assert!(has_summary, "discover should produce WorkspaceSummary");
}

// ============================================================================
// Utils unit tests
// ============================================================================

#[cfg(test)]
mod utils_tests {
    use hyperforge::services::utils::{dry_prefix, glob_match};

    #[test]
    fn glob_match_star() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn glob_match_prefix() {
        assert!(glob_match("plexus-*", "plexus-core"));
        assert!(glob_match("plexus-*", "plexus-"));
        assert!(!glob_match("plexus-*", "hyperforge"));
    }

    #[test]
    fn glob_match_suffix() {
        assert!(glob_match("*-core", "plexus-core"));
        assert!(!glob_match("*-core", "plexus-macros"));
    }

    #[test]
    fn glob_match_prefix_suffix() {
        assert!(glob_match("plexus-*-rs", "plexus-codegen-rs"));
        assert!(!glob_match("plexus-*-rs", "plexus-codegen-hs"));
    }

    #[test]
    fn glob_match_exact() {
        assert!(glob_match("alpha", "alpha"));
        assert!(!glob_match("alpha", "beta"));
    }

    #[test]
    fn dry_prefix_values() {
        assert_eq!(dry_prefix(true), "[DRY RUN] ");
        assert_eq!(dry_prefix(false), "");
    }
}

//! HYPERFORGE-57: `ci run` executes repos' `[ci]` runners through the
//! activation — one repo by path (a PERF-6-shaped custom level among them),
//! and a workspace with a result table and a non-zero end on failure.

use futures::StreamExt;
use hyperforge::v5::service::HyperforgeService;
use plexus_core::plexus::{Activation, PlexusStreamItem};
use serde_json::{json, Value};

async fn call(service: &HyperforgeService, method: &str, params: Value) -> Vec<Value> {
    let mut stream = service
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

fn of_type<'a>(events: &'a [Value], ty: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["type"] == ty).collect()
}

fn repo_in(parent: &std::path::Path, name: &str, config: &str) -> std::path::PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(dir.join(".hyperforge")).unwrap();
    std::fs::write(dir.join(".hyperforge/config.toml"), config).unwrap();
    dir
}

/// Shaped like plexus-core's: quick check, full build, a container, and a
/// custom local gate at index 3 calling a sibling checkout's script (PERF-6).
const LAYERED: &str = r#"
org = "o"

[ci]
skip_validate = false

[[ci.runners]]
type = "local"
build = ["echo", "quick"]
test = ["echo", "unit"]

[[ci.runners]]
type = "local"
build = ["echo", "full"]

[[ci.runners]]
type = "docker"
image = "rust:latest"
build = ["cargo", "build"]

[[ci.runners]]
type = "local"
build = ["../bench/ci-check.sh", "--build"]
test = ["../bench/ci-check.sh"]
timeout_secs = 60
"#;

const BROKEN: &str = r#"
[ci]
[[ci.runners]]
type = "local"
build = ["echo compiling; echo boom >&2; exit 7"]
test = ["echo never"]
"#;

fn bench(parent: &std::path::Path, verdict: i32) {
    use std::os::unix::fs::PermissionsExt;
    let dir = parent.join("bench");
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("ci-check.sh");
    std::fs::write(&script, format!("#!/bin/sh\necho \"perf gate $*\"\nexit {verdict}\n")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn one_repo_runs_its_local_runners_and_a_custom_level() {
    let config = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    bench(ws.path(), 0);
    let core = repo_in(ws.path(), "core", LAYERED);
    let service = HyperforgeService::with_state_dir(config.path().to_path_buf(), state.path());

    // Default: every local runner (0, 1, 3), build then test; docker skipped.
    let ev = call(&service, "ci.run", json!({ "path": core })).await;
    let done: Vec<_> = of_type(&ev, "runner_step").into_iter().filter(|e| e["status"] == "passed").collect();
    let seen: Vec<(u64, &str)> = done.iter().map(|e| (e["runner"].as_u64().unwrap(), e["step"].as_str().unwrap())).collect();
    assert_eq!(seen, [(0, "build"), (0, "test"), (1, "build"), (3, "build"), (3, "test")], "{ev:?}");
    let summary = of_type(&ev, "runner_summary");
    assert_eq!((summary[0]["passed"].clone(), summary[0]["failed"].clone()), (json!(1), json!(0)));
    assert!(of_type(&ev, "error").is_empty(), "{ev:?}");

    // --level 3: the PERF-6-shaped gate alone; a red gate fails the run.
    bench(ws.path(), 1);
    let ev = call(&service, "ci.run", json!({ "path": core, "level": 3 })).await;
    let steps = of_type(&ev, "runner_step");
    assert!(steps.iter().all(|e| e["runner"] == 3), "{ev:?}");
    let failed = steps.iter().find(|e| e["status"] == "failed").expect("a failed step");
    assert_eq!(failed["step"], "build");
    assert_eq!(failed["exit_code"], 1);
    assert_eq!(failed["output_tail"], "perf gate --build");
    assert_eq!(ev.last().unwrap()["type"], "error", "{ev:?}");
    assert_eq!(ev.last().unwrap()["code"], "ci_failed");

    // --level 2 --dry_run: the docker step listed, nothing run.
    let ev = call(&service, "ci.run", json!({ "path": core, "level": 2, "dry_run": true })).await;
    let steps = of_type(&ev, "runner_step");
    assert_eq!(steps.len(), 1, "{ev:?}");
    assert_eq!((steps[0]["kind"].clone(), steps[0]["status"].clone()), (json!("docker"), json!("dry_run")));

    // A level past the list, and a repo with no [ci], are refused.
    let ev = call(&service, "ci.run", json!({ "path": core, "level": 9 })).await;
    assert_eq!(ev[0]["code"], "validation", "{ev:?}");
    let bare = tempfile::tempdir().unwrap();
    let ev = call(&service, "ci.run", json!({ "path": bare.path() })).await;
    assert_eq!(ev[0]["code"], "no_ci", "{ev:?}");
    let ev = call(&service, "ci.run", json!({})).await;
    assert_eq!(ev[0]["code"], "validation", "{ev:?}");
}

#[tokio::test]
async fn a_workspace_run_gives_a_row_per_repo_and_fails_when_one_fails() {
    let config = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    bench(ws.path(), 0);
    repo_in(ws.path(), "core", LAYERED);
    repo_in(ws.path(), "broken", BROKEN);
    repo_in(ws.path(), "plain", "org = \"o\"\n");
    repo_in(ws.path(), "quiet", "[ci]\nskip_validate = true\n");
    std::fs::create_dir_all(config.path().join("workspaces")).unwrap();
    std::fs::write(
        config.path().join("workspaces/w.yaml"),
        format!(
            "name: w\npath: {}\nrepos:\n- o/core\n- o/broken\n- o/plain\n- o/quiet\n- o/gone\n",
            ws.path().display()
        ),
    )
    .unwrap();
    let service = HyperforgeService::with_state_dir(config.path().to_path_buf(), state.path());

    let ev = call(&service, "ci.run", json!({ "name": "w", "level": 0 })).await;
    let rows: Vec<(String, String)> = of_type(&ev, "runner_repo")
        .iter()
        .map(|e| (e["result"]["repo"].as_str().unwrap().to_string(), e["result"]["status"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(
        rows,
        [
            ("core".into(), "passed".into()),
            ("broken".into(), "failed".into()),
            ("plain".into(), "skipped".into()),
            ("quiet".into(), "skipped".into()),
            ("gone".into(), "skipped".into()),
        ],
        "{ev:?}"
    );
    // The broken build's test never ran; its output tail came back.
    let broken: Vec<_> = of_type(&ev, "runner_step").into_iter().filter(|e| e["repo"] == "broken").collect();
    assert!(broken.iter().all(|e| e["step"] == "build"), "{broken:?}");
    assert_eq!(broken.last().unwrap()["output_tail"], "compiling\nboom");
    let summary = of_type(&ev, "runner_summary")[0];
    assert_eq!((summary["passed"].clone(), summary["failed"].clone(), summary["skipped"].clone()), (json!(1), json!(1), json!(3)));
    let table = summary["table"].as_str().unwrap();
    assert!(table.starts_with("REPO"), "{table}");
    assert!(table.contains("broken") && table.contains("L0 build: exit 7"), "{table}");
    assert!(table.contains("no [ci]") && table.contains("skip_validate") && table.contains("no checkout"), "{table}");
    assert_eq!(ev.last().unwrap()["message"], "1 failed: broken");

    // include/exclude narrow the run.
    let ev = call(&service, "ci.run", json!({ "name": "w", "level": 0, "include": ["c*", "b*"], "exclude": ["broken"] })).await;
    assert_eq!(of_type(&ev, "runner_repo").len(), 1, "{ev:?}");
    assert!(of_type(&ev, "error").is_empty(), "{ev:?}");

    let ev = call(&service, "ci.run", json!({ "name": "nope" })).await;
    assert_eq!(ev[0]["code"], "not_found", "{ev:?}");
}

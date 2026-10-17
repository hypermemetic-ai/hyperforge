//! HYPERFORGE-22 acceptance 1: pipelines driven through the activation.
//!
//! A temp repo declares a two-stage pipeline (an auto `build` writing
//! `image=x`, a manual `deploy` echoing `$IMAGE`) and one whose build forgets
//! its output. Everything goes through `HyperforgeHub` by dotted method path,
//! as synapse sends it: `ci.add`, `ci.<pipeline>.start`, `….trigger`, ….

use futures::StreamExt;
use hyperforge::v5::hub::HyperforgeHub;
use plexus_core::plexus::{Activation, PlexusStreamItem};
use serde_json::{json, Value};

async fn call(hub: &HyperforgeHub, method: &str, params: Value) -> Vec<Value> {
    let mut stream = hub
        .call(method, params, None, None)
        .await
        .unwrap_or_else(|e| panic!("{method}: {e:?}"));
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        if let PlexusStreamItem::Data { content, .. } = item {
            out.push(content);
        }
    }
    out
}

fn of_type<'a>(events: &'a [Value], ty: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["type"] == ty).collect()
}

fn stage<'a>(run: &'a Value, name: &str) -> &'a Value {
    run["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no stage {name} in {run}"))
}

const PIPELINES: &str = r#"
[[pipeline]]
name = "app"
env = { IMAGE_STORE = "local" }

[[pipeline.stage]]
name = "build"
run = "echo building; echo image=x >> \"$HF_OUTPUTS\"; echo \"store=$IMAGE_STORE\" >> \"$HF_OUTPUTS\""
trigger = "auto"
outputs = ["image"]

[[pipeline.stage]]
name = "deploy"
needs = ["build"]
run = "echo \"deploying $IMAGE via $HF_BUILD_IMAGE attempt $HF_ATTEMPT\""

[[pipeline]]
name = "forgetful"

[[pipeline.stage]]
name = "build"
run = "echo no outputs here"
trigger = "auto"
outputs = ["image"]

[[pipeline.stage]]
name = "deploy"
needs = ["build"]
run = "echo never"

[[pipeline]]
name = "slow"

[[pipeline.stage]]
name = "wait"
run = "sleep 30; echo woke"
"#;

fn repo_with(text: &str) -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join(".hyperforge")).unwrap();
    std::fs::write(repo.path().join(".hyperforge/pipelines.toml"), text).unwrap();
    repo
}

#[tokio::test]
async fn pipelines_through_the_activation() {
    let config = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let repo = repo_with(PIPELINES);
    let hub = HyperforgeHub::with_state_dir(config.path().to_path_buf(), state.path());

    // ci add registers the repo; the path is kept in the config dir.
    let added = call(&hub, "ci.add", json!({ "path": repo.path() })).await;
    assert_eq!(added[0]["type"], "repo_added", "{added:?}");
    assert_eq!(added[0]["pipelines"], json!(["app", "forgetful", "slow"]));
    let kept = std::fs::read_to_string(config.path().join("ci.yaml")).unwrap();
    assert!(kept.contains(&std::fs::canonicalize(repo.path()).unwrap().display().to_string()));

    // Each pipeline is a child of ci, in the schema and by routing.
    let pipelines = call(&hub, "ci.pipelines", json!({})).await;
    assert_eq!(of_type(&pipelines, "pipeline").len(), 3);
    let described = call(&hub, "ci.app.describe", json!({})).await;
    assert_eq!(described[0]["stages"][0]["trigger"], "auto");
    assert_eq!(described[0]["stages"][1]["needs"], json!(["build"]));

    // start: the run id first, then the auto build runs and succeeds.
    let started = call(&hub, "ci.app.start", json!({ "by": "tester" })).await;
    assert_eq!(started[0]["type"], "run_started", "{started:?}");
    assert_eq!(started[0]["run"], 1);
    assert_eq!(started[0]["started_by"], "tester");
    let finished = of_type(&started, "stage_finished");
    assert_eq!(finished.len(), 1, "{started:?}");
    assert_eq!(finished[0]["stage"], "build");
    assert_eq!(finished[0]["state"], "succeeded");
    assert_eq!(finished[0]["outputs"]["image"], "x");
    assert_eq!(finished[0]["outputs"]["store"], "local");
    assert!(of_type(&started, "log").iter().any(|l| l["line"] == "building"));
    assert_eq!(started.last().unwrap()["type"], "idle");

    let run = call(&hub, "ci.app.get", json!({ "run": 1 })).await;
    assert_eq!(stage(&run[0]["run"], "build")["state"], "succeeded");
    assert_eq!(stage(&run[0]["run"], "deploy")["state"], "pending");

    // trigger: deploy runs with build's outputs from run 1.
    let deployed = call(&hub, "ci.app.trigger", json!({ "run": 1, "stage": "deploy" })).await;
    assert_eq!(deployed[0]["type"], "triggered", "{deployed:?}");
    assert_eq!(deployed[0]["attempt"], 1);
    assert!(
        of_type(&deployed, "log")
            .iter()
            .any(|l| l["line"] == "deploying x via x attempt 1"),
        "{deployed:?}"
    );
    assert_eq!(of_type(&deployed, "stage_finished")[0]["state"], "succeeded");

    // Again is a second attempt, and both are kept.
    let again = call(&hub, "ci.app.trigger", json!({ "run": 1, "stage": "deploy" })).await;
    assert_eq!(again[0]["attempt"], 2, "{again:?}");
    let run = call(&hub, "ci.app.get", json!({ "run": 1 })).await;
    let deploy = stage(&run[0]["run"], "deploy");
    assert_eq!(deploy["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(deploy["attempts"][1]["attempt"], 2);
    let log = call(&hub, "ci.app.log", json!({ "run": 1, "stage": "deploy" })).await;
    assert_eq!(log[0]["line"], "deploying x via x attempt 2");

    // A missing output fails its stage, naming it; a trigger whose need has
    // not succeeded is refused, naming the need and its state.
    let forgot = call(&hub, "ci.forgetful.start", json!({})).await;
    let f = of_type(&forgot, "stage_finished");
    assert_eq!(f[0]["state"], "failed", "{forgot:?}");
    assert!(f[0]["reason"].as_str().unwrap().contains("`image`"), "{forgot:?}");
    let refused = call(&hub, "ci.forgetful.trigger", json!({ "run": 1, "stage": "deploy" })).await;
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0]["type"], "error");
    let msg = refused[0]["message"].as_str().unwrap();
    assert!(msg.contains("`build`") && msg.contains("failed"), "{msg}");

    // cancel stops a running stage; it ends canceled.
    let slow = call(&hub, "ci.slow.start", json!({})).await;
    assert_eq!(slow[0]["run"], 1);
    let hub2 = hub.clone();
    let running = tokio::spawn(async move {
        call(&hub2, "ci.slow.trigger", json!({ "run": 1, "stage": "wait" })).await
    });
    let mut cancel = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel = call(&hub, "ci.slow.cancel", json!({ "run": 1, "stage": "wait" })).await;
        if cancel[0]["type"] == "cancel_requested" {
            break;
        }
    }
    assert_eq!(cancel[0]["type"], "cancel_requested", "{cancel:?}");
    let ran = tokio::time::timeout(std::time::Duration::from_secs(20), running)
        .await
        .expect("cancel stops the stage")
        .unwrap();
    assert_eq!(of_type(&ran, "stage_finished")[0]["state"], "canceled", "{ran:?}");

    // Runs survive a reload of the store; numbers are never reused.
    drop(hub);
    let hub = HyperforgeHub::with_state_dir(config.path().to_path_buf(), state.path());
    let runs = call(&hub, "ci.app.runs", json!({ "limit": 20 })).await;
    assert_eq!(runs.len(), 1);
    let deploy = stage(&runs[0]["run"], "deploy");
    assert_eq!(deploy["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(stage(&runs[0]["run"], "build")["attempts"][0]["outputs"]["image"], "x");
    let next = call(&hub, "ci.app.start", json!({})).await;
    assert_eq!(next[0]["run"], 2);
}

#[tokio::test]
async fn pipelines_appear_as_children_of_ci() {
    let config = tempfile::tempdir().unwrap();
    let repo = repo_with(PIPELINES);
    let hub = HyperforgeHub::new(config.path().to_path_buf());
    call(&hub, "ci.add", json!({ "path": repo.path() })).await;

    let root = hub.plugin_schema();
    let root = serde_json::to_value(&root).unwrap();
    assert!(root.to_string().contains("\"ci\""), "{root}");

    let schema = call(&hub, "ci.schema", json!({})).await;
    let text = serde_json::to_string(&schema).unwrap();
    for name in ["app", "forgetful", "slow", "pipelines", "add", "remove"] {
        assert!(text.contains(&format!("\"{name}\"")), "{name} missing from {text}");
    }
    let child = call(&hub, "ci.app.schema", json!({})).await;
    let text = serde_json::to_string(&child).unwrap();
    for m in ["describe", "start", "trigger", "runs", "get", "watch", "log", "cancel"] {
        assert!(text.contains(&format!("\"{m}\"")), "{m} missing from {text}");
    }
}

#[tokio::test]
async fn add_refuses_a_bad_file_and_a_taken_name() {
    let config = tempfile::tempdir().unwrap();
    let hub = HyperforgeHub::new(config.path().to_path_buf());

    let broken = repo_with("[[pipeline]]\nname = \"a\"\nrun = = 1\n");
    let r = call(&hub, "ci.add", json!({ "path": broken.path() })).await;
    assert_eq!(r[0]["type"], "error");
    assert!(r[0]["message"].as_str().unwrap().contains("line 3"), "{r:?}");

    let one = repo_with(PIPELINES);
    let two = repo_with("[[pipeline]]\nname = \"app\"\n");
    call(&hub, "ci.add", json!({ "path": one.path() })).await;
    let r = call(&hub, "ci.add", json!({ "path": two.path() })).await;
    let msg = r[0]["message"].as_str().unwrap();
    let one_path = std::fs::canonicalize(one.path()).unwrap();
    let two_path = std::fs::canonicalize(two.path()).unwrap();
    assert!(msg.contains("`app`"), "{msg}");
    assert!(msg.contains(&one_path.display().to_string()), "{msg}");
    assert!(msg.contains(&two_path.display().to_string()), "{msg}");

    let removed = call(&hub, "ci.remove", json!({ "path": one.path() })).await;
    assert_eq!(removed[0]["type"], "repo_removed");
    assert!(call(&hub, "ci.pipelines", json!({})).await.is_empty());
}

#[tokio::test]
async fn a_kebab_name_is_reached_as_synapse_sends_it() {
    // synapse turns `ci menger-app describe` into `ci.menger_app.describe`.
    let config = tempfile::tempdir().unwrap();
    let hub = HyperforgeHub::new(config.path().to_path_buf());
    let repo = repo_with(
        "[[pipeline]]\nname = \"menger-app\"\n[[pipeline.stage]]\nname = \"build\"\nrun = \"true\"\ntrigger = \"auto\"\n",
    );
    call(&hub, "ci.add", json!({ "path": repo.path() })).await;
    let d = call(&hub, "ci.menger_app.describe", json!({})).await;
    assert_eq!(d[0]["name"], "menger-app", "{d:?}");
    let s = call(&hub, "ci.menger_app.start", json!({})).await;
    assert_eq!(s[0]["pipeline"], "menger-app");
    assert_eq!(of_type(&s, "stage_finished")[0]["state"], "succeeded");
}

//! HYPERFORGE-23: a crate chain through the activation — `build.package_diff`,
//! `build.bump` and `build.publish` over a workspace of temp repos, against a
//! registry on disk (`HYPERFORGE_CRATES_INDEX` / `HYPERFORGE_CRATES_DL` as
//! `file://` directories). Everything goes through `HyperforgeService` by dotted
//! method path, as synapse sends it.
//!
//! The chain: `hf23t-client` + `hf23t-macros` (one cargo workspace, repo
//! `macros`), `hf23t-core` (repo `core`, needs both), `hf23t-api` +
//! `hf23t-app` (repo `app`, a workspace with a `[publish]` step of its own),
//! and `hf23t-sibling` (repo `sibling`, on core by `version = "0.6"`).

use std::path::{Path, PathBuf};
use std::process::Command;

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

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

const LIB: &str = "pub fn f() -> u32 { 1 }\n";
const META: &str = "edition = \"2021\"\nlicense = \"MIT\"\ndescription = \"hyperforge test crate\"\n";

/// The registry, once per test binary: `hf23t-core` 0.6.0 published from
/// today's source, `hf23t-macros` 0.6.0 from an older one.
fn registry() -> PathBuf {
    static ONCE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("hf23-registry-{}", std::process::id()));
        let entry = |name: &str, vers: &str| {
            format!("{{\"name\":\"{name}\",\"vers\":\"{vers}\",\"deps\":[],\"cksum\":\"0\",\"features\":{{}},\"yanked\":false}}\n")
        };
        let ipath = |n: &str| format!("{}/{}/{n}", &n[..2], &n[2..4]);
        write(&root.join("index").join(ipath("hf23t-core")), &entry("hf23t-core", "0.6.0"));
        write(&root.join("index").join(ipath("hf23t-macros")), &entry("hf23t-macros", "0.6.0"));
        let crate_file = |name: &str, files: &[(&str, &str)]| {
            let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut b = tar::Builder::new(gz);
            for (f, body) in files {
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, format!("{name}-0.6.0/{f}"), body.as_bytes()).unwrap();
            }
            let bytes = b.into_inner().unwrap().finish().unwrap();
            write(&root.join("dl").join(name).join(format!("{name}-0.6.0.crate")), "");
            std::fs::write(root.join("dl").join(name).join(format!("{name}-0.6.0.crate")), bytes).unwrap();
        };
        crate_file("hf23t-core", &[("Cargo.toml.orig", &core_manifest()), ("src/lib.rs", LIB)]);
        crate_file("hf23t-macros", &[("src/lib.rs", "pub fn f() -> u32 { 0 }\n")]);
        // Set once, before any service reads it; every test uses the same value.
        std::env::set_var("HYPERFORGE_CRATES_INDEX", format!("file://{}", root.join("index").display()));
        std::env::set_var("HYPERFORGE_CRATES_DL", format!("file://{}", root.join("dl").display()));
        root
    })
    .clone()
}

fn core_manifest() -> String {
    format!(
        "[package]\nname = \"hf23t-core\"\nversion = \"0.6.0\"\n{META}\n[dependencies]\nhf23t-macros = {{ path = \"../macros\", version = \"0.6.0\" }}  # the macros\nhf23t-client = {{ path = \"../macros/client\", version = \"0.6\" }}\n"
    )
}

/// The workspace: repos under `<root>/ws`, the config under `<root>/config`.
fn fixture(root: &Path, app_step: &str) -> (PathBuf, PathBuf) {
    registry();
    let ws = root.join("ws");
    write(
        &ws.join("macros/Cargo.toml"),
        &format!("[workspace]\nmembers = [\"client\"]\n\n[package]\nname = \"hf23t-macros\"\nversion = \"0.6.0\"\n{META}\n[dev-dependencies]\nhf23t-client = {{ path = \"client\", version = \"0.6.0\" }}\n"),
    );
    write(&ws.join("macros/src/lib.rs"), LIB);
    write(&ws.join("macros/client/Cargo.toml"), &format!("[package]\nname = \"hf23t-client\"\nversion = \"0.6.0\"\n{META}"));
    write(&ws.join("macros/client/src/lib.rs"), LIB);
    write(&ws.join("core/Cargo.toml"), &core_manifest());
    write(&ws.join("core/src/lib.rs"), LIB);
    write(
        &ws.join("app/Cargo.toml"),
        &format!("[workspace]\nmembers = [\".\", \"api\"]\n\n[package]\nname = \"hf23t-app\"\nversion = \"0.6.0\"\n{META}\n[dependencies]\nhf23t-api = {{ path = \"api\", version = \"0.6.0\" }}\nhf23t-core = {{ path = \"../core\", version = \"0.6\" }}\n"),
    );
    write(&ws.join("app/src/lib.rs"), LIB);
    write(
        &ws.join("app/api/Cargo.toml"),
        &format!("[package]\nname = \"hf23t-api\"\nversion = \"0.6.0\"\n{META}\n[dependencies]\nhf23t-core = {{ path = \"../../core\", version = \"0.6.0\" }}\n"),
    );
    write(&ws.join("app/api/src/lib.rs"), LIB);
    write(&ws.join("app/.hyperforge/config.toml"), app_step);
    write(
        &ws.join("sibling/Cargo.toml"),
        &format!("[package]\nname = \"hf23t-sibling\"\nversion = \"1.0.0\"\n{META}\n[dependencies]\nhf23t-core = {{ path = \"../core\", version = \"0.6\" }}\n# another line, from the registry: never moved\nhf23t-macros = \"0.5\"\n"),
    );
    write(&ws.join("sibling/src/lib.rs"), LIB);
    for r in ["macros", "core", "app", "sibling"] {
        let d = ws.join(r);
        write(&d.join(".gitignore"), "target/\nCargo.lock\n");
        git(&d, &["init", "-q", "-b", "main"]);
        git(&d, &["config", "user.email", "t@example.com"]);
        git(&d, &["config", "user.name", "t"]);
        git(&d, &["config", "tag.gpgSign", "false"]);
        git(&d, &["config", "commit.gpgSign", "false"]);
        git(&d, &["add", "-A"]);
        git(&d, &["commit", "-q", "-m", "init"]);
    }
    let config = root.join("config");
    write(
        &config.join("workspaces/chain.yaml"),
        &format!("name: chain\npath: {}\nrepos:\n- o/macros\n- o/core\n- o/app\n- o/sibling\n", ws.display()),
    );
    (ws, config)
}

const APP_STEP: &str = "[publish]\ncrates = [\"hf23t-api\", \"hf23t-app\"]\ndry_run = \"echo \\\"$HF_PUBLISH_MODE $HF_PUBLISH_CRATES\\\" > ../app-step.out; if [ -n \\\"$HF_CARGO_CONFIG\\\" ]; then cat \\\"$HF_CARGO_CONFIG\\\" >> ../app-step.out; fi\"\nexecute = \"echo \\\"$HF_PUBLISH_MODE $HF_PUBLISH_CRATES token=${CARGO_REGISTRY_TOKEN:+set}\\\" >> ../app-step.out\"\n";

#[tokio::test]
async fn package_diff_reads_the_registry_and_the_published_source() {
    let t = tempfile::tempdir().unwrap();
    let (_ws, config) = fixture(t.path(), APP_STEP);
    let service = HyperforgeService::new(config);
    let ev = call(&service, "build.package_diff", json!({"name": "chain", "include": ["hf23t-*"]})).await;
    let state = |n: &str| {
        of_type(&ev, "crate_state")
            .into_iter()
            .find(|e| e["name"] == n)
            .unwrap_or_else(|| panic!("no crate_state for {n}: {ev:#?}"))
            .clone()
    };
    assert_eq!(state("hf23t-core")["status"], "published", "{ev:#?}");
    let macros = state("hf23t-macros");
    assert_eq!(macros["status"], "drifted");
    assert_eq!(macros["changed"], json!(["src/lib.rs"]));
    assert_eq!(state("hf23t-app")["status"], "unpublished");
    // In dependency order: macros' workspace before core before app.
    let order: Vec<&str> = of_type(&ev, "crate_state").iter().map(|e| e["name"].as_str().unwrap()).collect();
    let pos = |n: &str| order.iter().position(|x| *x == n).unwrap();
    assert!(pos("hf23t-client") < pos("hf23t-core") && pos("hf23t-macros") < pos("hf23t-core"));
    assert!(pos("hf23t-core") < pos("hf23t-api"));
    // The refs form is still the manifests' diff.
    let ev = call(&service, "build.package_diff", json!({"name": "chain", "from_ref": "HEAD"})).await;
    assert_eq!(of_type(&ev, "error")[0]["code"], "validation");
}

#[tokio::test]
async fn bump_moves_members_and_requirements_one_commit_per_repo() {
    let t = tempfile::tempdir().unwrap();
    let (ws, config) = fixture(t.path(), APP_STEP);
    let service = HyperforgeService::new(config);
    let params = |dry: bool| json!({"name": "chain", "include": ["hf23t-client", "hf23t-macros", "hf23t-core"], "bump": "patch", "dry_run": dry});

    // A dry run writes nothing.
    let ev = call(&service, "build.bump", params(true)).await;
    assert_eq!(of_type(&ev, "crate_bumped").len(), 3, "{ev:#?}");
    assert!(std::fs::read_to_string(ws.join("core/Cargo.toml")).unwrap().contains("version = \"0.6.0\""));

    // A dirty manifest refuses the whole bump.
    write(&ws.join("core/Cargo.toml"), &format!("{}# local edit\n", core_manifest()));
    let ev = call(&service, "build.bump", params(false)).await;
    assert_eq!(of_type(&ev, "error")[0]["code"], "dirty", "{ev:#?}");
    assert!(std::fs::read_to_string(ws.join("macros/Cargo.toml")).unwrap().contains("version = \"0.6.0\""));
    git(&ws.join("core"), &["checkout", "--", "Cargo.toml"]);

    let ev = call(&service, "build.bump", params(false)).await;
    assert!(of_type(&ev, "error").is_empty(), "{ev:#?}");
    let core = std::fs::read_to_string(ws.join("core/Cargo.toml")).unwrap();
    assert!(core.contains("version = \"0.6.1\"\n"), "{core}");
    assert!(core.contains("hf23t-macros = { path = \"../macros\", version = \"0.6.1\" }  # the macros"), "{core}");
    assert!(core.contains("hf23t-client = { path = \"../macros/client\", version = \"0.6.1\" }"), "{core}");
    let macros = std::fs::read_to_string(ws.join("macros/Cargo.toml")).unwrap();
    assert!(macros.contains("hf23t-client = { path = \"client\", version = \"0.6.1\" }"), "{macros}");
    assert!(std::fs::read_to_string(ws.join("macros/client/Cargo.toml")).unwrap().contains("version = \"0.6.1\""));
    // Outside the set, a requirement that still matches stays.
    assert!(std::fs::read_to_string(ws.join("app/api/Cargo.toml")).unwrap().contains("version = \"0.6.0\" }"));
    assert!(std::fs::read_to_string(ws.join("sibling/Cargo.toml")).unwrap().contains("version = \"0.6\" }"));
    // One commit in each bumped repo, none elsewhere; trees clean.
    let commits = of_type(&ev, "bump_committed");
    let repos: Vec<&str> = commits.iter().map(|c| c["repo"].as_str().unwrap()).collect();
    assert_eq!(repos, ["o/core", "o/macros"], "{ev:#?}");
    for r in ["macros", "core"] {
        assert_eq!(git(&ws.join(r), &["rev-list", "--count", "HEAD"]).trim(), "2");
        assert_eq!(git(&ws.join(r), &["status", "--porcelain"]), "");
    }
    assert!(git(&ws.join("macros"), &["log", "-1", "--format=%s"]).contains("hf23t-client 0.6.0 → 0.6.1"));
    assert_eq!(git(&ws.join("app"), &["rev-list", "--count", "HEAD"]).trim(), "1");
    assert_eq!(git(&ws.join("sibling"), &["rev-list", "--count", "HEAD"]).trim(), "1");
    assert!(std::fs::read_to_string(ws.join("sibling/Cargo.toml")).unwrap().contains("hf23t-macros = \"0.5\"\n"));

    // A minor bump breaks "0.6" outside the set: the sibling follows.
    let ev = call(&service, "build.bump", json!({"name": "chain", "include": ["hf23t-core"], "bump": "minor"})).await;
    assert!(of_type(&ev, "error").is_empty(), "{ev:#?}");
    let sib = std::fs::read_to_string(ws.join("sibling/Cargo.toml")).unwrap();
    assert!(sib.contains("hf23t-core = { path = \"../core\", version = \"0.7.0\" }"), "{sib}");
    assert!(git(&ws.join("sibling"), &["log", "-1", "--format=%s"]).starts_with("chore(deps)"));
}

#[tokio::test]
async fn publish_dry_runs_the_chain_together_then_executes_in_order() {
    let t = tempfile::tempdir().unwrap();
    let (ws, config) = fixture(t.path(), APP_STEP);
    std::env::set_var("CARGO_TARGET_DIR", t.path().join("target"));
    let service = HyperforgeService::with_state_dir(config, &t.path().join("state"));

    // core 0.6.0 is on the registry (skipped); the rest are not.
    let ev = call(&service, "build.publish", json!({"name": "chain", "include": ["hf23t-app"]})).await;
    let plan: Vec<(String, String)> = of_type(&ev, "publish_planned")
        .iter()
        .map(|e| (e["name"].as_str().unwrap().to_string(), e["action"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(
        plan,
        [
            ("hf23t-client".to_string(), "publish".to_string()),
            ("hf23t-macros".into(), "skip".into()),
            ("hf23t-core".into(), "skip".into()),
            ("hf23t-api".into(), "publish".into()),
            ("hf23t-app".into(), "publish".into()),
        ],
        "{ev:#?}"
    );
    let groups = of_type(&ev, "publish_group");
    let done: Vec<(&str, &str)> = groups
        .iter()
        .filter(|g| g["state"] != "started")
        .map(|g| (g["repo"].as_str().unwrap(), g["state"].as_str().unwrap()))
        .collect();
    assert_eq!(done, [("o/macros", "ok"), ("o/app", "ok")], "{ev:#?}");
    assert!(groups[0]["step"].as_str().unwrap().starts_with("cargo publish --dry-run --allow-dirty -p hf23t-client"));
    // The app's own step ran in dry-run mode, with a patch for client.
    let out = std::fs::read_to_string(ws.join("app-step.out")).unwrap();
    assert!(out.starts_with("dry-run hf23t-api hf23t-app\n[patch.crates-io]\n\"hf23t-client\""), "{out}");
    let summary = of_type(&ev, "publish_summary")[0];
    assert_eq!(summary["execute"], false);
    assert_eq!(summary["verified"], 3);
    assert_eq!(summary["published"], json!([]));

    // Execute refuses a dirty crate before anything runs.
    write(&ws.join("app/api/src/extra.rs"), "");
    let ev = call(&service, "build.publish", json!({"name": "chain", "include": ["hf23t-a*"], "closure": false, "execute": true})).await;
    assert_eq!(of_type(&ev, "error")[0]["code"], "dirty", "{ev:#?}");
    assert!(of_type(&ev, "publish_group").is_empty());
    std::fs::remove_file(ws.join("app/api/src/extra.rs")).unwrap();

    // Execute: the dry runs again, then the step for real; the crates tagged.
    let ev = call(&service, "build.publish", json!({"name": "chain", "include": ["hf23t-a*"], "closure": false, "execute": true})).await;
    assert!(of_type(&ev, "error").is_empty(), "{ev:#?}");
    let phases: Vec<&str> = of_type(&ev, "publish_group").iter().filter(|g| g["state"] == "ok").map(|g| g["phase"].as_str().unwrap()).collect();
    assert_eq!(phases, ["dry_run", "execute"]);
    assert!(std::fs::read_to_string(ws.join("app-step.out")).unwrap().ends_with("execute hf23t-api hf23t-app token=\n"));
    assert_eq!(of_type(&ev, "publish_summary")[0]["published"], json!(["hf23t-api", "hf23t-app"]));
    let tags = git(&ws.join("app"), &["tag", "--list"]);
    assert_eq!(tags, "hf23t-api-v0.6.0\nhf23t-app-v0.6.0\n");
}

#[tokio::test]
async fn a_failed_dry_run_publishes_nothing() {
    let t = tempfile::tempdir().unwrap();
    let step = "[publish]\ndry_run = \"echo broken >&2; exit 3\"\nexecute = \"touch ../executed\"\n";
    let (ws, config) = fixture(t.path(), step);
    let service = HyperforgeService::with_state_dir(config, &t.path().join("state"));
    let ev = call(&service, "build.publish", json!({"name": "chain", "include": ["hf23t-a*"], "closure": false, "execute": true})).await;
    let e = of_type(&ev, "error");
    assert_eq!(e[0]["code"], "dry_run_failed", "{ev:#?}");
    let failed = of_type(&ev, "publish_group").into_iter().find(|g| g["state"] == "failed").unwrap();
    assert!(failed["tail"].as_str().unwrap().contains("broken"));
    assert!(!ws.join("executed").exists());
    assert_eq!(of_type(&ev, "publish_summary")[0]["published"], json!([]));
    // HYPERFORGE-62 (5): without the closure, a dependency neither in the run
    // nor on the registry refuses the publish before anything runs.
    let ev = call(&service, "build.publish", json!({"name": "chain", "include": ["hf23t-app"], "closure": false})).await;
    let e = of_type(&ev, "error");
    assert_eq!(e[0]["code"], "unpublished_dependency", "{ev:#?}");
    assert!(e[0]["message"].as_str().unwrap().contains("hf23t-app needs hf23t-api 0.6.0"), "{ev:#?}");
    assert!(of_type(&ev, "publish_group").is_empty());
    // include is required.
    let ev = call(&service, "build.publish", json!({"name": "chain"})).await;
    assert_eq!(of_type(&ev, "error")[0]["code"], "validation");
}

fn cargo(dir: &Path, args: &[&str]) {
    let out = Command::new("cargo").current_dir(dir).args(args).output().unwrap();
    assert!(out.status.success(), "cargo {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// HYPERFORGE-62 (4) and (2), with the real cargo: `hf62t-top` needs
/// `hf62t-mid` (another repo), which needs `hf62t-base` — a member of
/// top's own workspace. Three groups, split / mid / split: top's dry run
/// resolves base through the patch (it is not on crates.io), and the dry
/// runs leave every tracked `Cargo.lock` as it was.
#[tokio::test]
async fn a_crate_needing_an_earlier_group_of_its_own_workspace_dry_runs_and_leaves_trees_clean() {
    let t = tempfile::tempdir().unwrap();
    registry();
    std::env::set_var("CARGO_TARGET_DIR", t.path().join("target"));
    let ws = t.path().join("ws");
    write(
        &ws.join("split/Cargo.toml"),
        &format!("[workspace]\nmembers = [\"base\"]\n\n[package]\nname = \"hf62t-top\"\nversion = \"0.1.0\"\n{META}\n[dependencies]\nhf62t-base = {{ path = \"base\", version = \"0.1.0\" }}\nhf62t-mid = {{ path = \"../mid\", version = \"0.1.0\" }}\n"),
    );
    write(&ws.join("split/src/lib.rs"), LIB);
    write(&ws.join("split/base/Cargo.toml"), &format!("[package]\nname = \"hf62t-base\"\nversion = \"0.1.0\"\n{META}"));
    write(&ws.join("split/base/src/lib.rs"), LIB);
    write(
        &ws.join("mid/Cargo.toml"),
        &format!("[package]\nname = \"hf62t-mid\"\nversion = \"0.1.0\"\n{META}\n[dependencies]\nhf62t-base = {{ path = \"../split/base\", version = \"0.1.0\" }}\n"),
    );
    write(&ws.join("mid/src/lib.rs"), LIB);
    for r in ["split", "mid"] {
        let d = ws.join(r);
        // Cargo.lock is tracked here: the dry run must not move it.
        write(&d.join(".gitignore"), "target/\n");
        cargo(&d, &["generate-lockfile", "--offline"]);
        git(&d, &["init", "-q", "-b", "main"]);
        git(&d, &["config", "user.email", "t@example.com"]);
        git(&d, &["config", "user.name", "t"]);
        git(&d, &["config", "commit.gpgSign", "false"]);
        git(&d, &["add", "-A"]);
        git(&d, &["commit", "-q", "-m", "init"]);
    }
    let config = t.path().join("config");
    write(
        &config.join("workspaces/traps.yaml"),
        &format!("name: traps\npath: {}\nrepos:\n- o/split\n- o/mid\n", ws.display()),
    );
    let service = HyperforgeService::with_state_dir(config, &t.path().join("state"));
    let ev = call(&service, "build.publish", json!({"name": "traps", "include": ["hf62t-top"]})).await;
    let order: Vec<&str> = of_type(&ev, "publish_planned").iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(order, ["hf62t-base", "hf62t-mid", "hf62t-top"], "{ev:#?}");
    let done: Vec<(&str, &str)> = of_type(&ev, "publish_group")
        .iter()
        .filter(|g| g["state"] != "started")
        .map(|g| (g["repo"].as_str().unwrap(), g["state"].as_str().unwrap()))
        .collect();
    assert_eq!(done, [("o/split", "ok"), ("o/mid", "ok"), ("o/split", "ok")], "{ev:#?}");
    assert!(of_type(&ev, "error").is_empty(), "{ev:#?}");
    // top's patch names base, of its own workspace.
    let last = of_type(&ev, "publish_group").into_iter().rfind(|g| g["state"] == "ok").unwrap();
    let step = last["step"].as_str().unwrap();
    let patch = step.split("--config ").nth(1).expect("top's dry run has a patch");
    assert!(std::fs::read_to_string(patch).unwrap().contains("\"hf62t-base\""), "{step}");
    assert_eq!(of_type(&ev, "publish_summary")[0]["verified"], 3, "{ev:#?}");
    for r in ["split", "mid"] {
        assert_eq!(git(&ws.join(r), &["status", "--porcelain"]), "", "{r} after the dry run");
    }
}

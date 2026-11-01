//! HYPERFORGE-26: Cabal repos through `build.publish` and `build.package_diff`,
//! by dotted method path as synapse sends them. Nothing here can reach a real
//! registry: a fake `cabal` and a fake `cargo` are first on PATH for the whole
//! binary, and Hackage and crates.io are `file://` directories
//! (`HYPERFORGE_HACKAGE_URL`, `HYPERFORGE_CRATES_INDEX`).
//!
//! The fakes write what they were asked to `../fake-tool.log`, beside the
//! repo. `../fail-check-<pkg>` and `../fail-upload-<pkg>` make them fail.

use std::path::{Path, PathBuf};
use std::process::Command;

use futures::StreamExt;
use hyperforge::v5::service::HyperforgeService;
use plexus_core::plexus::{Activation, PlexusStreamItem};
use serde_json::{json, Value};

const TOKEN: &str = "hf26-fake-token-not-real";

const FAKE_CABAL: &str = r#"#!/bin/sh
name=$(sed -n 's/^name: *//p' *.cabal)
version=$(sed -n 's/^version: *//p' *.cabal)
printf 'cabal %s\n' "$*" >> ../fake-tool.log
case "$1" in
  check)
    if [ -e "../fail-check-$name" ]; then echo 'Error: [bad] Hackage would reject this'; exit 1; fi
    echo 'Warning: [no-repository] encouraged to specify source control'
    echo 'Warning: [no-description] no description field'
    ;;
  sdist)
    while [ $# -gt 0 ]; do
      if [ "$1" = "-o" ]; then out="$2"; fi
      shift
    done
    mkdir -p "$out"
    echo tarball > "$out/$name-$version.tar.gz"
    echo 'Wrote tarball sdist to'
    echo "$out/$name-$version.tar.gz"
    ;;
  upload)
    printf 'upload-env name=%s config=%s mode=%s hackage-token=%s\n' "$name" "$CABAL_CONFIG" \
      "$(stat -c %a "$CABAL_CONFIG" 2>/dev/null)" "${HACKAGE_TOKEN:+set}" >> ../fake-tool.log
    [ -n "$CABAL_CONFIG" ] && cp "$CABAL_CONFIG" "../upload-config-$name"
    if [ -e "../fail-upload-$name" ]; then echo 'upload refused by the fake' >&2; exit 1; fi
    ;;
esac
"#;

const FAKE_CARGO: &str = r#"#!/bin/sh
printf 'cargo %s\n' "$*" >> ../fake-tool.log
printf 'cargo-token=%s\n' "${CARGO_REGISTRY_TOKEN:+set}" >> ../fake-tool.log
if [ -e ../fail-cargo ]; then echo 'cargo refused by the fake' >&2; exit 1; fi
for a in "$@"; do
  if [ "$prev" = "--config" ]; then cat "$a" >> ../fake-tool.log; fi
  prev="$a"
done
"#;

const FAKE_NPM: &str = r#"#!/bin/sh
printf 'npm %s\n' "$*" >> ../fake-tool.log
printf 'npm-token=%s\n' "${NPM_TOKEN:+set}" >> ../fake-tool.log
if [ -n "$NPM_CONFIG_USERCONFIG" ]; then cat "$NPM_CONFIG_USERCONFIG" >> ../fake-tool.log; fi
"#;

const FAKE_TWINE: &str = r#"#!/bin/sh
printf 'twine %s\n' "$*" >> ../fake-tool.log
printf 'twine-token=%s\n' "${TWINE_PASSWORD:+set}" >> ../fake-tool.log
"#;

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn cabal_file(name: &str, version: &str, deps: &str) -> String {
    format!(
        "cabal-version: 3.0\nname: {name}\nversion: {version}\nsynopsis: test\n\nlibrary\n  exposed-modules: Lib\n  hs-source-dirs: src\n  build-depends:\n    base >= 4 && < 5,\n{deps}\n  default-language: Haskell2010\n"
    )
}

const LIB_HS: &str = "module Lib where\n";

fn targz(prefix: &str, files: &[(&str, &str)]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut b = tar::Builder::new(gz);
    for (f, body) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, format!("{prefix}/{f}"), body.as_bytes()).unwrap();
    }
    b.into_inner().unwrap().finish().unwrap()
}

/// Once per test binary: the fakes on PATH, a Hackage and a crates.io on
/// disk. Hackage holds `hf26-held` 1.0.0.0 (and an older line, and a
/// deprecated one) and `hf26-proto` 0.5.2.0.
fn env() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("hf26-env-{}", std::process::id()));
        let bin = root.join("bin");
        for (name, body) in [("cabal", FAKE_CABAL), ("cargo", FAKE_CARGO), ("npm", FAKE_NPM), ("twine", FAKE_TWINE)] {
            write(&bin.join(name), body);
            let chmod = Command::new("chmod").arg("+x").arg(bin.join(name)).status().unwrap();
            assert!(chmod.success());
        }
        let hackage = root.join("hackage");
        write(
            &hackage.join("package/hf26-held/preferred"),
            r#"{"normal-version":["1.0.0.0","0.9.0.0"],"deprecated-version":["0.8.0.0"]}"#,
        );
        write(&hackage.join("package/hf26-proto/preferred"), r#"{"normal-version":["0.5.2.0"]}"#);
        let tarball = hackage.join("package/hf26-held-1.0.0.0/hf26-held-1.0.0.0.tar.gz");
        write(&tarball, "");
        let manifest = cabal_file("hf26-held", "1.0.0.0", "    text");
        let bytes = targz("hf26-held-1.0.0.0", &[("hf26-held.cabal", &manifest), ("src/Lib.hs", LIB_HS)]);
        std::fs::write(tarball, bytes).unwrap();
        std::fs::create_dir_all(root.join("index")).unwrap();
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{path}", bin.display()));
        std::env::set_var("HYPERFORGE_HACKAGE_URL", format!("file://{}", hackage.display()));
        std::env::set_var("HYPERFORGE_CRATES_INDEX", format!("file://{}", root.join("index").display()));
        std::env::set_var("HYPERFORGE_CRATES_DL", format!("file://{}", root.join("dl").display()));
        std::env::set_var("HOME", root.join("home"));
        std::env::remove_var("HACKAGE_TOKEN");
        std::env::remove_var("CARGO_REGISTRY_TOKEN");
    });
}

fn commit(dir: &Path) {
    write(&dir.join(".gitignore"), "dist-newstyle/\n");
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["config", "tag.gpgSign", "false"]);
    git(dir, &["config", "commit.gpgSign", "false"]);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn cabal_repo(ws: &Path, repo: &str, name: &str, version: &str, deps: &str) {
    let d = ws.join(repo);
    write(&d.join(format!("{name}.cabal")), &cabal_file(name, version, deps));
    write(&d.join("src/Lib.hs"), LIB_HS);
    commit(&d);
}

/// `proto` (`hf26-proto` 0.5.3.0) and `synapse` (`hf26-synapse`, the name
/// differs from the repo), which needs it; `repos` adds more to the list.
struct Fx {
    ws: PathBuf,
    state: PathBuf,
    config: PathBuf,
    service: HyperforgeService,
    _tmp: tempfile::TempDir,
}

fn fixture(extra: impl FnOnce(&Path) -> Vec<&'static str>) -> Fx {
    env();
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    // Listed dependent first: the order must come from the dependencies.
    cabal_repo(&ws, "synapse", "hf26-synapse", "3.0.0", "    hf26-proto >= 0.5 && < 0.6,\n    text");
    cabal_repo(&ws, "proto", "hf26-proto", "0.5.3.0", "    text");
    let mut repos = vec!["synapse", "proto"];
    repos.extend(extra(&ws));
    let listed: String = repos.iter().map(|r| format!("- o/{r}\n")).collect();
    let config = tmp.path().join("config");
    write(
        &config.join("workspaces/w.yaml"),
        &format!("name: w\npath: {}\nrepos:\n{listed}", ws.display()),
    );
    write(&config.join("secrets.yaml"), &format!("hackage/token: {TOKEN}\ncargo/token: cargo-fake\n"));
    let state = tmp.path().join("state");
    let service = HyperforgeService::with_state_dir(config.clone(), &state);
    Fx { ws, state, config, service, _tmp: tmp }
}

impl Fx {
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

    fn tool_log(&self) -> String {
        std::fs::read_to_string(self.ws.join("fake-tool.log")).unwrap_or_default()
    }

    fn tool_lines(&self, prefix: &str) -> Vec<String> {
        self.tool_log().lines().filter(|l| l.starts_with(prefix)).map(str::to_string).collect()
    }
}

fn of_type<'a>(events: &'a [Value], ty: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["type"] == ty).collect()
}

fn plan(ev: &[Value]) -> Vec<(String, String)> {
    of_type(ev, "publish_planned")
        .iter()
        .map(|e| (e["name"].as_str().unwrap().to_string(), e["action"].as_str().unwrap().to_string()))
        .collect()
}

fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_string(), b.to_string())
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().is_dir() {
            files_under(&e.path(), out);
        } else {
            out.push(e.path());
        }
    }
}

#[tokio::test]
async fn dry_run_checks_and_builds_uploads_nothing() {
    let fx = fixture(|_| vec![]);
    let ev = fx.call("build.publish", json!({"name": "w", "include": ["hf26-proto"]})).await;
    assert!(of_type(&ev, "error").is_empty(), "{ev:#?}");
    assert_eq!(plan(&ev), [pair("hf26-proto", "publish")], "{ev:#?}");
    let log = fx.tool_lines("cabal ");
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(log[0], "cabal check");
    assert!(log[1].starts_with("cabal sdist --ignore-project -o "), "{log:?}");
    // The same event shapes as cargo: started, then ok, a step and a log.
    let g = of_type(&ev, "publish_group");
    assert_eq!(g.len(), 2, "{ev:#?}");
    assert_eq!(g[0]["state"], "started");
    assert_eq!(g[1]["state"], "ok");
    assert_eq!(g[1]["phase"], "dry_run");
    assert_eq!(g[1]["repo"], "o/proto");
    assert_eq!(g[1]["crates"], json!(["hf26-proto"]));
    assert!(g[1]["step"].as_str().unwrap().contains("cabal check"), "{ev:#?}");
    // A warning is not a failure, and it is said.
    let notes = of_type(&ev, "chain_note");
    assert!(notes.iter().any(|n| n["message"].as_str().unwrap().contains("2 warnings")), "{ev:#?}");
    let s = of_type(&ev, "publish_summary")[0];
    assert_eq!(s["execute"], false);
    assert_eq!(s["verified"], 1);
    assert_eq!(s["published"], json!([]));
    assert_eq!(git(&fx.ws.join("proto"), &["tag", "--list"]), "");
}

#[tokio::test]
async fn one_repo_form_finds_cabal_and_honours_dry_run() {
    let fx = fixture(|_| vec![]);
    // The form that failed live: no channel, a repo with no Cargo.toml.
    let ev = fx.call("build.publish", json!({"org": "o", "name": "proto"})).await;
    assert!(of_type(&ev, "error").is_empty(), "{ev:#?}");
    assert_eq!(of_type(&ev, "publish_summary")[0]["verified"], 1, "{ev:#?}");
    assert!(fx.tool_lines("cabal upload").is_empty());
    assert!(fx.tool_lines("cargo").is_empty());
    // The repo's name is enough; the package's differs; no closure here.
    let ev = fx.call("build.publish", json!({"org": "o", "name": "synapse", "channel": "hackage"})).await;
    assert_eq!(plan(&ev), [pair("hf26-synapse", "publish")], "{ev:#?}");
    // Execute uploads.
    let ev = fx.call("build.publish", json!({"org": "o", "name": "proto", "execute": true})).await;
    assert!(of_type(&ev, "error").is_empty(), "{ev:#?}");
    assert_eq!(fx.tool_lines("cabal upload").len(), 1);
    assert_eq!(of_type(&ev, "publish_summary")[0]["published"], json!(["hf26-proto"]));
    // A channel nothing answers to says so, and hackage needs a .cabal.
    let ev = fx.call("build.publish", json!({"org": "o", "name": "proto", "channel": "maven"})).await;
    assert_eq!(of_type(&ev, "error")[0]["code"], "unknown_channel", "{ev:#?}");
}

#[tokio::test]
async fn hackage_channel_refuses_a_repo_with_no_cabal_file() {
    let fx = fixture(|ws| {
        let d = ws.join("rusty");
        write(&d.join("Cargo.toml"), "[package]\nname = \"hf26-rusty\"\nversion = \"1.0.0\"\n");
        commit(&d);
        vec!["rusty"]
    });
    let ev = fx.call("build.publish", json!({"org": "o", "name": "rusty", "channel": "hackage"})).await;
    assert_eq!(of_type(&ev, "error")[0]["code"], "manifest_not_found", "{ev:#?}");
    assert!(fx.tool_log().is_empty());
}

#[tokio::test]
async fn execute_hands_the_token_over_a_private_file_never_argv() {
    let fx = fixture(|_| vec![]);
    let ev = fx.call("build.publish", json!({"name": "w", "include": ["hf26-proto"], "execute": true})).await;
    assert!(of_type(&ev, "error").is_empty(), "{ev:#?}");
    let log = fx.tool_log();
    // No argv, no fake's record of one, no event carries it.
    assert!(!log.contains(TOKEN), "{log}");
    assert!(!serde_json::to_string(&ev).unwrap().contains(TOKEN));
    let uploads = fx.tool_lines("cabal upload");
    assert_eq!(uploads.len(), 1, "{log}");
    let tarball = uploads[0].strip_prefix("cabal upload --publish ").expect("argv is upload --publish <tarball>");
    assert!(tarball.ends_with("/sdist/hf26-proto-0.5.3.0.tar.gz"), "{tarball}");
    // It uploads what the dry run built, in this run's log dir.
    assert!(Path::new(tarball).is_file(), "{tarball}");
    assert!(fx.tool_lines("cabal sdist")[0].ends_with(Path::new(tarball).parent().unwrap().to_str().unwrap()));
    // cabal finds it in a 0600 config, gone afterwards; no env var carries it.
    let env = fx.tool_lines("upload-env");
    assert_eq!(env.len(), 1, "{log}");
    assert!(env[0].contains("mode=600"), "{env:?}");
    assert!(env[0].ends_with("hackage-token="), "{env:?}");
    let config = std::fs::read_to_string(fx.ws.join("upload-config-hf26-proto")).unwrap();
    assert!(config.contains(&format!("\ntoken: {TOKEN}\n")), "{config}");
    assert!(config.contains("url: https://hackage.haskell.org/"), "{config}");
    let path = env[0].split("config=").nth(1).unwrap().split(' ').next().unwrap();
    assert!(!Path::new(path).exists(), "{path} left behind");
    // Nothing under the run's logs holds it.
    let mut files = Vec::new();
    files_under(&fx.state, &mut files);
    assert!(!files.is_empty());
    for f in files {
        assert!(!std::fs::read(&f).unwrap().windows(TOKEN.len()).any(|w| w == TOKEN.as_bytes()), "{}", f.display());
    }
    // Tagged as cargo is; summary likewise.
    assert_eq!(git(&fx.ws.join("proto"), &["tag", "--list"]), "hf26-proto-v0.5.3.0\n");
    assert_eq!(of_type(&ev, "crate_tagged")[0]["tag"], "hf26-proto-v0.5.3.0");
    assert_eq!(of_type(&ev, "publish_summary")[0]["published"], json!(["hf26-proto"]));
}

#[tokio::test]
async fn a_version_hackage_holds_is_skipped() {
    let fx = fixture(|ws| {
        cabal_repo(ws, "held", "hf26-held", "1.0.0.0", "    text");
        cabal_repo(ws, "old", "hf26-old", "0.1.0.0", "    hf26-held");
        vec!["held", "old"]
    });
    let ev = fx.call("build.publish", json!({"name": "w", "include": ["hf26-old"], "execute": true})).await;
    assert_eq!(plan(&ev), [pair("hf26-held", "skip"), pair("hf26-old", "publish")], "{ev:#?}");
    let skip = of_type(&ev, "publish_planned")[0];
    assert!(skip["reason"].as_str().unwrap().contains("Hackage holds hf26-held 1.0.0.0"), "{ev:#?}");
    let uploads = fx.tool_lines("cabal upload");
    assert_eq!(uploads.len(), 1, "{uploads:?}");
    assert!(uploads[0].contains("hf26-old-0.1.0.0"), "{uploads:?}");
    assert_eq!(git(&fx.ws.join("held"), &["tag", "--list"]), "");
    // All held: nothing runs at all.
    let ev = fx.call("build.publish", json!({"name": "w", "include": ["hf26-held"], "closure": false, "execute": true})).await;
    assert!(of_type(&ev, "publish_group").is_empty(), "{ev:#?}");
    assert_eq!(of_type(&ev, "publish_summary")[0]["skipped"], 1);
}

#[tokio::test]
async fn a_failed_check_publishes_nothing_of_the_chain() {
    let fx = fixture(|_| vec![]);
    write(&fx.ws.join("fail-check-hf26-synapse"), "");
    let ev = fx.call("build.publish", json!({"name": "w", "include": ["hf26-synapse"], "execute": true})).await;
    assert_eq!(of_type(&ev, "error")[0]["code"], "dry_run_failed", "{ev:#?}");
    // Every group is dry-run, as for cargo: proto too, though synapse failed.
    let done: Vec<(&str, &str)> = of_type(&ev, "publish_group")
        .iter()
        .filter(|g| g["state"] != "started")
        .map(|g| (g["repo"].as_str().unwrap(), g["state"].as_str().unwrap()))
        .collect();
    assert_eq!(done, [("o/proto", "ok"), ("o/synapse", "failed")], "{ev:#?}");
    let failed = of_type(&ev, "publish_group").into_iter().find(|g| g["state"] == "failed").unwrap();
    assert!(failed["tail"].as_str().unwrap().contains("Hackage would reject"), "{ev:#?}");
    assert!(fx.tool_lines("cabal upload").is_empty());
    // An error in check stops that group before its sdist.
    assert_eq!(fx.tool_lines("cabal sdist").len(), 1);
    assert_eq!(of_type(&ev, "publish_summary")[0]["published"], json!([]));
}

#[tokio::test]
async fn a_failed_upload_stops_the_chain() {
    let fx = fixture(|_| vec![]);
    write(&fx.ws.join("fail-upload-hf26-proto"), "");
    let ev = fx.call("build.publish", json!({"name": "w", "include": ["hf26-synapse"], "execute": true})).await;
    assert_eq!(of_type(&ev, "error")[0]["code"], "publish_failed", "{ev:#?}");
    let uploads = fx.tool_lines("cabal upload");
    assert_eq!(uploads.len(), 1, "{uploads:?}");
    assert!(uploads[0].contains("hf26-proto"), "{uploads:?}");
    let s = of_type(&ev, "publish_summary")[0];
    assert_eq!(s["published"], json!([]));
    assert_eq!(s["failed"], json!(["hf26-proto"]));
    assert_eq!(s["not_published"], json!(["hf26-proto", "hf26-synapse"]));
    assert_eq!(git(&fx.ws.join("proto"), &["tag", "--list"]), "");
    // The first through, the second refused: half-published, the first tagged.
    std::fs::remove_file(fx.ws.join("fail-upload-hf26-proto")).unwrap();
    write(&fx.ws.join("fail-upload-hf26-synapse"), "");
    let ev = fx.call("build.publish", json!({"name": "w", "include": ["hf26-synapse"], "execute": true})).await;
    assert_eq!(of_type(&ev, "error")[0]["code"], "half_published", "{ev:#?}");
    assert_eq!(of_type(&ev, "publish_summary")[0]["published"], json!(["hf26-proto"]));
    assert_eq!(git(&fx.ws.join("proto"), &["tag", "--list"]), "hf26-proto-v0.5.3.0\n");
    assert_eq!(git(&fx.ws.join("synapse"), &["tag", "--list"]), "");
}

#[tokio::test]
async fn dependencies_order_and_the_repo_name_selects() {
    let fx = fixture(|_| vec![]);
    let want = [pair("hf26-proto", "publish"), pair("hf26-synapse", "publish")];
    let ev = fx.call("build.publish", json!({"name": "w", "include": ["hf26-synapse"]})).await;
    assert_eq!(plan(&ev), want, "{ev:#?}");
    assert!(of_type(&ev, "chain_note").iter().any(|n| n["message"] == "hf26-proto added: hf26-synapse needs it"), "{ev:#?}");
    // `synapse` is the repo, `hf26-synapse` the package: either selects it.
    let ev = fx.call("build.publish", json!({"name": "w", "include": ["synapse"]})).await;
    assert_eq!(plan(&ev), want, "{ev:#?}");
    assert!(of_type(&ev, "chain_note").iter().any(|n| n["message"] == "include synapse: the repo of hf26-synapse"), "{ev:#?}");
    // And it excludes as it includes.
    let ev = fx.call("build.publish", json!({"name": "w", "include": ["hf26-*"], "exclude": ["synapse"]})).await;
    assert_eq!(plan(&ev), [pair("hf26-proto", "publish")], "{ev:#?}");
    // A name nothing has is still refused.
    let ev = fx.call("build.publish", json!({"name": "w", "include": ["nope"]})).await;
    assert_eq!(of_type(&ev, "error")[0]["code"], "validation", "{ev:#?}");
}

fn rust_repos(ws: &Path) -> Vec<&'static str> {
    const META: &str = "edition = \"2021\"\nlicense = \"MIT\"\ndescription = \"t\"\n";
    write(
        &ws.join("rusty/Cargo.toml"),
        &format!("[workspace]\nmembers = [\"b\"]\n\n[package]\nname = \"hf26-a\"\nversion = \"1.0.0\"\n{META}"),
    );
    write(&ws.join("rusty/src/lib.rs"), "");
    write(&ws.join("rusty/b/Cargo.toml"), &format!("[package]\nname = \"hf26-b\"\nversion = \"1.0.0\"\n{META}"));
    write(&ws.join("rusty/b/src/lib.rs"), "");
    commit(&ws.join("rusty"));
    write(
        &ws.join("tail/Cargo.toml"),
        &format!("[package]\nname = \"hf26-tail\"\nversion = \"1.0.0\"\n{META}\n[dependencies]\nhf26-a = {{ path = \"../rusty\", version = \"1.0.0\" }}\n"),
    );
    write(&ws.join("tail/src/lib.rs"), "");
    commit(&ws.join("tail"));
    vec!["rusty", "tail"]
}

#[tokio::test]
async fn a_mixed_workspace_keeps_its_cargo_groups() {
    let fx = fixture(rust_repos);
    let include = json!(["hf26-tail", "hf26-b", "hf26-synapse"]);
    let ev = fx.call("build.publish", json!({"name": "w", "include": include})).await;
    assert!(of_type(&ev, "error").is_empty(), "{ev:#?}");
    assert_eq!(of_type(&ev, "publish_summary")[0]["verified"], 5, "{ev:#?}");
    let cargo = fx.tool_lines("cargo publish");
    assert_eq!(cargo.len(), 2, "{cargo:?}");
    // One group for the cargo workspace, as before the Cabal packages joined.
    assert!(cargo[0].starts_with("cargo publish --dry-run --allow-dirty -p hf26-a -p hf26-b --config "), "{cargo:?}");
    assert!(cargo[1].starts_with("cargo publish --dry-run --allow-dirty -p hf26-tail --config "), "{cargo:?}");
    // The patch names the cargo crates only, never a Haskell checkout.
    let log = fx.tool_log();
    assert!(log.contains("[patch.crates-io]\n\"hf26-a\""), "{log}");
    assert!(!log.contains("\"hf26-proto\""), "{log}");
    assert!(!log.contains("\"hf26-synapse\""), "{log}");
    // Every group's dry run, Cabal and Cargo, before any publish.
    let ev = fx.call("build.publish", json!({"name": "w", "include": include, "execute": true})).await;
    assert!(of_type(&ev, "error").is_empty(), "{ev:#?}");
    let phases: Vec<&str> = of_type(&ev, "publish_group")
        .iter()
        .filter(|g| g["state"] == "ok")
        .map(|g| g["phase"].as_str().unwrap())
        .collect();
    assert_eq!(phases, ["dry_run", "dry_run", "dry_run", "dry_run", "execute", "execute", "execute", "execute"], "{ev:#?}");
    let log = fx.tool_log();
    assert!(log.contains("cargo publish -p hf26-a -p hf26-b\ncargo-token=set\n"), "{log}");
    assert!(log.contains("cargo publish -p hf26-tail\ncargo-token=set\n"), "{log}");
    assert_eq!(fx.tool_lines("cabal upload").len(), 2, "{log}");
    assert!(of_type(&ev, "publish_summary")[0]["failed"] == json!([]), "{ev:#?}");
    assert_eq!(git(&fx.ws.join("rusty"), &["tag", "--list"]), "hf26-a-v1.0.0\nhf26-b-v1.0.0\n");
}

#[tokio::test]
async fn execute_refuses_uncommitted_changes() {
    let fx = fixture(|_| vec![]);
    write(&fx.ws.join("proto/src/Extra.hs"), "module Extra where\n");
    let ev = fx.call("build.publish", json!({"name": "w", "include": ["hf26-synapse"], "execute": true})).await;
    let e = of_type(&ev, "error");
    assert_eq!(e[0]["code"], "dirty", "{ev:#?}");
    assert!(e[0]["message"].as_str().unwrap().contains("hf26-proto"), "{ev:#?}");
    assert!(of_type(&ev, "publish_group").is_empty());
    assert!(fx.tool_log().is_empty());
    // A dry run goes ahead, as for cargo.
    let ev = fx.call("build.publish", json!({"name": "w", "include": ["hf26-synapse"]})).await;
    assert_eq!(of_type(&ev, "publish_summary")[0]["verified"], 2, "{ev:#?}");
}

#[tokio::test]
async fn package_diff_reads_hackage_with_pvp_versions() {
    let fx = fixture(|ws| {
        cabal_repo(ws, "held", "hf26-held", "1.0.0.0", "    text");
        vec!["held"]
    });
    let state = |ev: &[Value], n: &str| {
        of_type(ev, "crate_state")
            .into_iter()
            .find(|e| e["name"] == n)
            .unwrap_or_else(|| panic!("no crate_state for {n}: {ev:#?}"))
            .clone()
    };
    let ev = fx.call("build.package_diff", json!({"name": "w"})).await;
    assert!(of_type(&ev, "error").is_empty(), "{ev:#?}");
    assert_eq!(state(&ev, "hf26-synapse")["status"], "unpublished");
    let proto = state(&ev, "hf26-proto");
    assert_eq!(proto["status"], "ahead", "{ev:#?}");
    assert_eq!(proto["latest"], "0.5.2.0");
    assert_eq!(proto["repo"], "o/proto");
    let held = state(&ev, "hf26-held");
    assert_eq!(held["status"], "published", "{ev:#?}");
    assert_eq!(held["latest"], "1.0.0.0");
    assert_eq!(held["changed_count"], 0);
    // In dependency order.
    let order: Vec<&str> = of_type(&ev, "crate_state").iter().map(|e| e["name"].as_str().unwrap()).collect();
    let pos = |n: &str| order.iter().position(|x| *x == n).unwrap();
    assert!(pos("hf26-proto") < pos("hf26-synapse"), "{order:?}");
    // The source moved since the published tarball.
    write(&fx.ws.join("held/src/Lib.hs"), "module Lib where\n-- moved\n");
    let ev = fx.call("build.package_diff", json!({"name": "w", "include": ["hf26-held"]})).await;
    let held = state(&ev, "hf26-held");
    assert_eq!(held["status"], "drifted", "{ev:#?}");
    assert_eq!(held["changed"], json!(["src/Lib.hs"]));
    // Behind: a 4-part version below Hackage's latest; a deprecated one is yanked.
    let manifest = fx.ws.join("held/hf26-held.cabal");
    write(&manifest, &cabal_file("hf26-held", "0.5.0.0", "    text"));
    let ev = fx.call("build.package_diff", json!({"name": "w", "include": ["hf26-held"]})).await;
    assert_eq!(state(&ev, "hf26-held")["status"], "behind", "{ev:#?}");
    write(&manifest, &cabal_file("hf26-held", "0.8.0.0", "    text"));
    let ev = fx.call("build.package_diff", json!({"name": "w", "include": ["hf26-held"]})).await;
    assert_eq!(state(&ev, "hf26-held")["status"], "yanked", "{ev:#?}");
}

#[tokio::test]
async fn bump_says_what_it_leaves_and_touches_no_cabal_file() {
    let fx = fixture(rust_repos);
    // Asked for by name: refused, not silently done.
    let ev = fx.call("build.bump", json!({"name": "w", "include": ["hf26-proto"], "bump": "patch"})).await;
    assert_eq!(of_type(&ev, "error")[0]["code"], "unsupported", "{ev:#?}");
    // Caught by a glob: the cargo crates move, the Cabal ones are named.
    let ev = fx.call("build.bump", json!({"name": "w", "include": ["hf26-*"], "bump": "patch"})).await;
    assert!(of_type(&ev, "error").is_empty(), "{ev:#?}");
    let bumped: Vec<&str> = of_type(&ev, "crate_bumped").iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(bumped.len(), 3, "{ev:#?}");
    assert!(bumped.iter().all(|n| !n.contains("proto") && !n.contains("synapse")), "{bumped:?}");
    assert!(of_type(&ev, "chain_note").iter().any(|n| n["message"].as_str().unwrap().contains("hf26-proto")), "{ev:#?}");
    assert_eq!(git(&fx.ws.join("proto"), &["status", "--porcelain"]), "");
    assert_eq!(git(&fx.ws.join("proto"), &["rev-list", "--count", "HEAD"]).trim(), "1");
}

/// One repo, every channel: without `--execute` the channel's dry run and no
/// token; with it the real command. (channel, dry argv, execute argv.)
#[tokio::test]
async fn one_repo_publish_only_dry_runs_without_execute() {
    let cases = [
        ("crates.io", "cargo publish --dry-run --allow-dirty", "cargo publish --allow-dirty", "cargo-token="),
        ("npm", "npm publish --dry-run", "npm publish", "npm-token="),
        ("pypi", "twine check dist/*", "twine upload dist/*", "twine-token="),
    ];
    for (channel, dry, real, token_line) in cases {
        let fx = fixture(rust_repos);
        for params in [
            json!({"org": "o", "name": "rusty", "channel": channel}),
            json!({"org": "o", "name": "rusty", "channel": channel, "execute": false}),
        ] {
            let ev = fx.call("build.publish", params).await;
            assert!(of_type(&ev, "error").is_empty(), "{channel}: {ev:#?}");
            assert!(of_type(&ev, "package_published").is_empty(), "{channel}: {ev:#?}");
            let g = of_type(&ev, "publish_group");
            assert_eq!(g.len(), 1, "{channel}: {ev:#?}");
            assert_eq!(g[0]["phase"], "dry_run");
            assert_eq!(g[0]["state"], "ok");
            assert_eq!(g[0]["step"], dry);
        }
        let log = fx.tool_log();
        let tool = dry.split(' ').next().unwrap();
        assert_eq!(fx.tool_lines(&format!("{tool} ")), [dry, dry], "{log}");
        assert_eq!(fx.tool_lines(token_line), [token_line, token_line], "no token in a dry run: {log}");

        // Nobody's token is needed for a dry run; execute needs its own.
        let ev = fx.call("build.publish", json!({"org": "o", "name": "rusty", "channel": channel, "execute": true})).await;
        if channel == "crates.io" {
            assert!(of_type(&ev, "error").is_empty(), "{ev:#?}");
        } else {
            assert_eq!(of_type(&ev, "error")[0]["code"], "missing_token", "{ev:#?}");
            let secrets = fx.config.join("secrets.yaml");
            write(&secrets, "cargo/token: f\nnpm/token: f\npypi/token: f\n");
            let ev = fx.call("build.publish", json!({"org": "o", "name": "rusty", "channel": channel, "execute": true})).await;
            assert!(of_type(&ev, "error").is_empty(), "{ev:#?}");
            assert_eq!(of_type(&ev, "package_published").len(), 1, "{ev:#?}");
        }
        let lines = fx.tool_lines(&format!("{tool} "));
        assert_eq!(lines.last().unwrap(), real, "{lines:?}");
        assert_eq!(lines.iter().filter(|l| *l == real).count(), 1, "{lines:?}");
        assert_eq!(fx.tool_lines(&format!("{token_line}set")).len(), 1, "{}", fx.tool_log());
    }
}

#[tokio::test]
async fn one_repo_cargo_is_the_default_and_a_failed_dry_run_says_so() {
    let fx = fixture(rust_repos);
    // No channel on a cargo repo: crates.io, still only a dry run.
    let ev = fx.call("build.publish", json!({"org": "o", "name": "rusty"})).await;
    assert_eq!(fx.tool_lines("cargo publish"), ["cargo publish --dry-run --allow-dirty"], "{ev:#?}");
    write(&fx.ws.join("fail-cargo"), "");
    let ev = fx.call("build.publish", json!({"org": "o", "name": "rusty"})).await;
    let e = of_type(&ev, "error");
    assert_eq!(e[0]["code"], "dry_run_failed", "{ev:#?}");
    assert!(e[0]["message"].as_str().unwrap().contains("refused by the fake"), "{ev:#?}");
    assert!(of_type(&ev, "package_published").is_empty());
}

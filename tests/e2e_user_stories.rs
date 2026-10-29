//! End-to-end user-story tests for the ORGS epic.
//!
//! See `plans/ORGS/ORGS-4.md` — this file is the executable form of the
//! epic's user stories, not method-level coverage. Each `test_usN_*`
//! function corresponds to a user story in the epic overview (`ORGS-1.md`)
//! and exercises the composition of `orgs_add`, `orgs_update`,
//! `orgs_list`, and `orgs_delete` the way a real operator would chain them.
//!
//! Method-level success / error / dry-run behavior lives in
//! `integration_test.rs`. The purpose of *these* tests is to answer
//! "did this epic deliver the workflows it was motivated by?" — so the
//! assertions are on end states (on-disk TOML, what `orgs_list`
//! reports, schema shape) rather than on the exact event discriminators
//! of each intermediate call.
//!
//! These tests exercise the shipped Rust service surface, not the Synapse
//! CLI — ORGS-4 describes the scenarios in terms of `synapse` commands
//! but a Rust test cannot practically spawn Synapse processes. Routing
//! JSON via `DynamicService::route("hyperforge.<method>", …, None)` hits
//! exactly the same code paths a Synapse call would reach, so it is
//! the correct in-process equivalent. US-6 (`--help` discoverability) is
//! tested via the JsonSchema the service exposes: the shape of the schema
//! is what Synapse consumes to emit per-forge `--ssh.<forge>` flags.
//!
//! Each test runs in its own `tempfile::TempDir` config dir via
//! `HyperforgeService::new_with_config_dir`, so tests do not contaminate
//! each other or the developer's real `~/.config/hyperforge`.

use futures::StreamExt;
use hyperforge::{HyperforgeEvent, HyperforgeService};
use plexus_core::plexus::{Activation, DynamicHub as DynamicService, PlexusStreamItem};
use std::sync::Arc;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Build a service rooted at a tempdir config directory. Returns (service, tempdir).
/// The tempdir must be held by the caller for the duration of the test so
/// its directory is not deleted before the service finishes using it.
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
            if let Ok(event) = serde_json::from_value::<HyperforgeEvent>(content) {
                events.push(event);
            }
        }
    }
    events
}

/// Read the on-disk org TOML for `org` in the given tempdir as a
/// generic `toml::Value` so we can make structural assertions.
fn read_org_toml(tmp: &TempDir, org: &str) -> toml::Value {
    let p = tmp.path().join("orgs").join(format!("{}.toml", org));
    let raw = std::fs::read_to_string(&p).unwrap_or_default();
    toml::from_str(&raw).unwrap_or(toml::Value::Table(Default::default()))
}

/// Extract `ssh.<forge>` from an org's on-disk TOML, if present.
fn ssh_value(tmp: &TempDir, org: &str, forge: &str) -> Option<String> {
    read_org_toml(tmp, org)
        .get("ssh")
        .and_then(|v| v.get(forge))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Extract top-level `workspace_path` from an org's on-disk TOML, if present.
fn ws_path_value(tmp: &TempDir, org: &str) -> Option<String> {
    read_org_toml(tmp, org)
        .get("workspace_path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// True if the org's config TOML exists on disk.
fn org_config_exists(tmp: &TempDir, org: &str) -> bool {
    tmp.path()
        .join("orgs")
        .join(format!("{}.toml", org))
        .exists()
}

/// Return the `orgs_list` output line for a specific org, if listed. The
/// line format is "  <org> — workspace: ..., forges: [...]".
async fn orgs_list_line(service: &DynamicService, org: &str) -> Option<String> {
    let events = call(service, "hyperforge.orgs_list", serde_json::json!({})).await;
    events.into_iter().find_map(|e| match e {
        HyperforgeEvent::Info { message }
            if message.trim_start().starts_with(&format!("{} ", org))
                || message.trim_start().starts_with(&format!("{}\u{2014}", org))
                || message.contains(&format!(" {} \u{2014} ", org)) =>
        {
            Some(message)
        }
        _ => None,
    })
}

/// Return the set of org names reported by `orgs_list` (excluding the
/// trailing "N org(s) configured." summary).
async fn list_org_names(service: &DynamicService) -> Vec<String> {
    let events = call(service, "hyperforge.orgs_list", serde_json::json!({})).await;
    let mut out = Vec::new();
    for ev in events {
        if let HyperforgeEvent::Info { message } = ev {
            let trimmed = message.trim_start();
            if trimmed.contains(" org(s) configured") {
                continue;
            }
            if let Some(dash_idx) = trimmed.find(" \u{2014} ") {
                let name = trimmed[..dash_idx].trim();
                if !name.is_empty() {
                    out.push(name.to_string());
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// US-1 — Onboard a new org with SSH + workspace
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_us1_onboard() {
    let (service, tmp) = test_service();

    // Hold keys/workspace paths as tempdir-anchored paths so they look
    // like a real onboarding flow.
    let key_path = tmp.path().join("keys").join("gh");
    let ws_path = tmp.path().join("dev").join("testorg");
    let key_str = key_path.to_string_lossy().to_string();
    let ws_str = ws_path.to_string_lossy().to_string();

    // orgs_add with ssh.github + workspace_path.
    let add = call(
        &service,
        "hyperforge.orgs_add",
        serde_json::json!({
            "org": "testorg",
            "ssh": { "github": key_str },
            "workspace_path": ws_str,
        }),
    )
    .await;
    assert!(
        add.iter()
            .any(|e| matches!(e, HyperforgeEvent::OrgAdded { org, dry_run: false } if org == "testorg")),
        "orgs_add should report a non-dry-run OrgAdded, got {:?}",
        add,
    );

    // Post-state assertion 1: file exists with expected fields on disk.
    assert!(org_config_exists(&tmp, "testorg"), "testorg.toml must exist");
    assert_eq!(ssh_value(&tmp, "testorg", "github").as_deref(), Some(key_str.as_str()));
    assert_eq!(ws_path_value(&tmp, "testorg").as_deref(), Some(ws_str.as_str()));

    // Post-state assertion 2: orgs_list reports testorg with the fields.
    let line = orgs_list_line(&service, "testorg")
        .await
        .expect("orgs_list should include testorg");
    assert!(line.contains(&ws_str), "workspace_path missing from listing: {}", line);
    assert!(line.contains("github"), "github forge missing from listing: {}", line);
}

// ---------------------------------------------------------------------------
// US-2 — Rotate an SSH key (merge semantics)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_us2_rotate_ssh() {
    let (service, tmp) = test_service();

    // Seed with github + codeberg keys.
    call(
        &service,
        "hyperforge.orgs_add",
        serde_json::json!({
            "org": "testorg",
            "ssh": { "github": "/a", "codeberg": "/b" },
        }),
    )
    .await;

    // Rotate the github key via orgs_update — default is per-field merge.
    let upd = call(
        &service,
        "hyperforge.orgs_update",
        serde_json::json!({
            "org": "testorg",
            "ssh": { "github": "/c" },
        }),
    )
    .await;
    assert!(
        upd.iter()
            .any(|e| matches!(e, HyperforgeEvent::OrgUpdated { org, dry_run: false, .. } if org == "testorg")),
        "orgs_update should succeed, got {:?}",
        upd,
    );

    // Post-state: github rotated to /c, codeberg untouched at /b.
    assert_eq!(ssh_value(&tmp, "testorg", "github").as_deref(), Some("/c"));
    assert_eq!(
        ssh_value(&tmp, "testorg", "codeberg").as_deref(),
        Some("/b"),
        "codeberg key must not be touched by a github-only update",
    );
}

// ---------------------------------------------------------------------------
// US-3 — Re-home and then clear `workspace_path`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_us3_rehome_and_clear_workspace() {
    let (service, tmp) = test_service();

    // Seed the org with an initial workspace.
    call(
        &service,
        "hyperforge.orgs_add",
        serde_json::json!({
            "org": "testorg",
            "ssh": { "github": "/k" },
            "workspace_path": "/old/path",
        }),
    )
    .await;

    // 1. Dry-run: propose re-home to /new/path, but assert no persistence.
    let pre_dry = std::fs::read_to_string(tmp.path().join("orgs").join("testorg.toml")).unwrap();
    let dry = call(
        &service,
        "hyperforge.orgs_update",
        serde_json::json!({
            "org": "testorg",
            "workspace_path": "/new/path",
            "dry_run": true,
        }),
    )
    .await;
    assert!(
        dry.iter()
            .any(|e| matches!(e, HyperforgeEvent::OrgUpdated { dry_run: true, .. })),
        "dry-run should emit OrgUpdated with dry_run=true, got {:?}",
        dry,
    );
    let post_dry = std::fs::read_to_string(tmp.path().join("orgs").join("testorg.toml")).unwrap();
    assert_eq!(pre_dry, post_dry, "dry-run must not mutate on-disk TOML");
    assert_eq!(ws_path_value(&tmp, "testorg").as_deref(), Some("/old/path"));

    // 2. Commit the re-home.
    call(
        &service,
        "hyperforge.orgs_update",
        serde_json::json!({
            "org": "testorg",
            "workspace_path": "/new/path",
        }),
    )
    .await;
    assert_eq!(ws_path_value(&tmp, "testorg").as_deref(), Some("/new/path"));

    // 3. Clear via empty string.
    call(
        &service,
        "hyperforge.orgs_update",
        serde_json::json!({
            "org": "testorg",
            "workspace_path": "",
        }),
    )
    .await;
    assert_eq!(
        ws_path_value(&tmp, "testorg"),
        None,
        "empty-string workspace_path must leave the field absent in serialized TOML",
    );
    // `skip_serializing_if = \"Option::is_none\"` — the key itself should be gone.
    let raw = std::fs::read_to_string(tmp.path().join("orgs").join("testorg.toml")).unwrap();
    assert!(
        !raw.contains("workspace_path"),
        "serialized TOML must not contain a workspace_path line after clear, got:\n{}",
        raw,
    );
}

// ---------------------------------------------------------------------------
// US-4 — Preview before writing (dry-run)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_us4_preview_before_writing() {
    let (service, tmp) = test_service();

    let events = call(
        &service,
        "hyperforge.orgs_add",
        serde_json::json!({
            "org": "typoed",
            "ssh": { "github": "/x" },
            "dry_run": true,
        }),
    )
    .await;

    // Post-state 1: the file does NOT exist.
    assert!(
        !org_config_exists(&tmp, "typoed"),
        "dry-run must not write the org TOML",
    );

    // Post-state 2: the event stream contains a preview event that names
    // the intended write. `OrgAdded { dry_run: true }` is the natural
    // signal; assert on the org name appearing in the stream.
    let mentions_typoed = events.iter().any(|e| match e {
        HyperforgeEvent::OrgAdded { org, dry_run } => *dry_run && org == "typoed",
        HyperforgeEvent::Info { message } => message.contains("typoed"),
        _ => false,
    });
    assert!(
        mentions_typoed,
        "dry-run event stream must reference the intended org name, got {:?}",
        events,
    );
}

// ---------------------------------------------------------------------------
// US-5 — Scripted idempotent bootstrap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_us5_scripted_idempotent_bootstrap() {
    let (service, tmp) = test_service();

    // The scripted shell form of this story uses `jq -e` to short-circuit
    // adds for orgs that already exist. In a Rust test we replicate the
    // short-circuit by consulting orgs_list before each add — that is the
    // programmatic equivalent of the shell guard.
    //
    // The invariant being tested: looping twice reaches the same end
    // state, with no test assertion failing between the loops.

    async fn ensure_org(service: &DynamicService, name: &str, key: &str) {
        let existing = list_org_names(service).await;
        if existing.iter().any(|o| o == name) {
            return;
        }
        call(
            service,
            "hyperforge.orgs_add",
            serde_json::json!({
                "org": name,
                "ssh": { "github": key },
            }),
        )
        .await;
    }

    // First loop — both should be added.
    ensure_org(&service, "alpha", "/keys/alpha").await;
    ensure_org(&service, "beta", "/keys/beta").await;

    assert!(org_config_exists(&tmp, "alpha"));
    assert!(org_config_exists(&tmp, "beta"));
    let after_first = list_org_names(&service).await;
    assert!(after_first.contains(&"alpha".to_string()));
    assert!(after_first.contains(&"beta".to_string()));

    // Snapshot mtimes to prove the files are not re-written on the second
    // loop.
    let alpha_path = tmp.path().join("orgs").join("alpha.toml");
    let beta_path = tmp.path().join("orgs").join("beta.toml");
    let alpha_mtime = std::fs::metadata(&alpha_path).unwrap().modified().unwrap();
    let beta_mtime = std::fs::metadata(&beta_path).unwrap().modified().unwrap();

    // Make any touch visible.
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Second loop — the guard must short-circuit; no writes happen.
    ensure_org(&service, "alpha", "/keys/alpha").await;
    ensure_org(&service, "beta", "/keys/beta").await;

    assert_eq!(
        std::fs::metadata(&alpha_path).unwrap().modified().unwrap(),
        alpha_mtime,
        "idempotent bootstrap must not rewrite alpha.toml on the second loop",
    );
    assert_eq!(
        std::fs::metadata(&beta_path).unwrap().modified().unwrap(),
        beta_mtime,
        "idempotent bootstrap must not rewrite beta.toml on the second loop",
    );

    // If a caller skipped the guard and called orgs_add twice directly,
    // the second call must still produce a distinguishable failure event
    // rather than silently succeeding — so scripts that forget the guard
    // can still tell what happened.
    let retry = call(
        &service,
        "hyperforge.orgs_add",
        serde_json::json!({
            "org": "alpha",
            "ssh": { "github": "/keys/alpha" },
        }),
    )
    .await;
    let had_failure = retry
        .iter()
        .any(|e| matches!(e, HyperforgeEvent::OrgAddFailed { .. }));
    assert!(
        had_failure,
        "duplicate orgs_add (without guard) must emit OrgAddFailed so scripts can detect it, got {:?}",
        retry,
    );
}

// ---------------------------------------------------------------------------
// US-6 — `--help` discoverability (via service JsonSchema)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_us6_help_discoverability() {
    // This story says: `synapse ... orgs_add --help` should show per-forge
    // SSH flags (`--ssh.github`, `--ssh.codeberg`, `--ssh.gitlab`) and
    // NOT a single `--ssh` flag that accepts JSON. Synapse derives that
    // flag layout from the JsonSchema the service exposes for the method's
    // params. So the in-process equivalent of the `--help` check is an
    // inspection of the schema's `ssh` property: it must be an object
    // with typed per-forge fields, not an opaque string/blob.

    let hyperforge = HyperforgeService::new();
    let schema = Activation::plugin_schema(&hyperforge);

    for method_name in ["orgs_add", "orgs_update"] {
        let method = schema
            .methods
            .iter()
            .find(|m| m.name == method_name)
            .unwrap_or_else(|| panic!("{} missing from plugin_schema", method_name));

        let params = method
            .params
            .as_ref()
            .unwrap_or_else(|| panic!("{} params schema missing", method_name));
        let params_json = serde_json::to_value(params)
            .unwrap_or_else(|_| panic!("{} params schema not serializable", method_name));

        // Drill into `ssh`. With schemars, Option<OrgSshKeys> appears as
        // a property under `properties.ssh`. The schema may be inlined
        // or emitted behind a $ref — tolerate both forms.
        let ssh_schema = resolve_property(&params_json, "ssh").unwrap_or_else(|| {
            panic!(
                "{}: params schema has no `ssh` property, got: {}",
                method_name,
                serde_json::to_string_pretty(&params_json).unwrap(),
            )
        });

        // The schema must describe ssh as an object with per-forge
        // properties. Three distinct properties, one per known forge.
        let ssh_props = find_properties(&ssh_schema).unwrap_or_else(|| {
            panic!(
                "{}: ssh schema has no object properties — Synapse would emit a single --ssh JSON flag. Schema: {}",
                method_name,
                serde_json::to_string_pretty(&ssh_schema).unwrap(),
            )
        });

        for forge in ["github", "codeberg", "gitlab"] {
            assert!(
                ssh_props.contains_key(forge),
                "{}: ssh schema missing typed `{}` field — Synapse cannot emit --ssh.{} flag. Got keys: {:?}",
                method_name,
                forge,
                forge,
                ssh_props.keys().collect::<Vec<_>>(),
            );
        }

        // And the ssh schema must not be a primitive string — that would
        // collapse to a single `--ssh` JSON-blob flag.
        if let Some(ty) = ssh_schema.get("type").and_then(|v| v.as_str()) {
            assert_ne!(
                ty, "string",
                "{}: ssh must not be a string type (would render as a single --ssh flag)",
                method_name,
            );
        }
    }
}

/// Resolve `properties.<name>` in a params schema, following a single
/// `$ref` into `$defs` if the property is emitted by reference.
fn resolve_property(schema: &serde_json::Value, name: &str) -> Option<serde_json::Value> {
    let props = schema.get("properties")?.as_object()?;
    let raw = props.get(name)?;
    Some(resolve_ref(schema, raw))
}

/// If `v` is a `$ref: "#/$defs/Foo"`, return the referenced definition
/// from the root schema. Otherwise return `v` unchanged. Also merges
/// `allOf`/`anyOf` branches containing a single `$ref`, which is how
/// schemars emits `Option<T>` in many cases.
fn resolve_ref(root: &serde_json::Value, v: &serde_json::Value) -> serde_json::Value {
    // Direct $ref.
    if let Some(r) = v.get("$ref").and_then(|r| r.as_str()) {
        if let Some(def_name) = r.strip_prefix("#/$defs/") {
            if let Some(def) = root
                .get("$defs")
                .and_then(|d| d.get(def_name))
            {
                return def.clone();
            }
        }
        if let Some(def_name) = r.strip_prefix("#/definitions/") {
            if let Some(def) = root
                .get("definitions")
                .and_then(|d| d.get(def_name))
            {
                return def.clone();
            }
        }
    }

    // Option<T> is often an anyOf of [T, null] or allOf containing a $ref.
    for key in ["anyOf", "allOf", "oneOf"] {
        if let Some(arr) = v.get(key).and_then(|x| x.as_array()) {
            for variant in arr {
                // Skip the null branch.
                if variant.get("type").and_then(|t| t.as_str()) == Some("null") {
                    continue;
                }
                // Recurse into any variant that carries structure or a $ref.
                let resolved = resolve_ref(root, variant);
                if resolved.get("properties").is_some() || resolved.get("$ref").is_some() {
                    return resolved;
                }
            }
        }
    }

    v.clone()
}

/// Return the `properties` map of a (possibly nested) schema object.
fn find_properties(
    schema: &serde_json::Value,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    schema.get("properties").and_then(|p| p.as_object()).cloned()
}

// ---------------------------------------------------------------------------
// Anti-story — rename via delete+add composes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_antistory_rename_composes() {
    let (service, tmp) = test_service();

    // 1. Create the "old" org with a full shape.
    call(
        &service,
        "hyperforge.orgs_add",
        serde_json::json!({
            "org": "oldname",
            "ssh": { "github": "/a" },
            "workspace_path": "/w",
        }),
    )
    .await;
    assert!(org_config_exists(&tmp, "oldname"));

    // 2. Capture the shape from disk (the "jq .oldname" equivalent).
    let captured_github = ssh_value(&tmp, "oldname", "github");
    let captured_ws = ws_path_value(&tmp, "oldname");
    assert_eq!(captured_github.as_deref(), Some("/a"));
    assert_eq!(captured_ws.as_deref(), Some("/w"));

    // 3. Create the "new" org with the captured shape.
    let mut ssh_map = serde_json::Map::new();
    if let Some(gh) = &captured_github {
        ssh_map.insert("github".to_string(), serde_json::Value::String(gh.clone()));
    }
    let mut params = serde_json::Map::new();
    params.insert("org".to_string(), serde_json::Value::String("newname".to_string()));
    params.insert("ssh".to_string(), serde_json::Value::Object(ssh_map));
    if let Some(wp) = &captured_ws {
        params.insert(
            "workspace_path".to_string(),
            serde_json::Value::String(wp.clone()),
        );
    }
    let add = call(&service, "hyperforge.orgs_add", serde_json::Value::Object(params)).await;
    assert!(
        add.iter()
            .any(|e| matches!(e, HyperforgeEvent::OrgAdded { org, dry_run: false } if org == "newname")),
        "add of newname should succeed, got {:?}",
        add,
    );

    // 4. Delete the old org (confirm: true — orgs_delete dry-runs otherwise).
    call(
        &service,
        "hyperforge.orgs_delete",
        serde_json::json!({
            "org": "oldname",
            "confirm": true,
        }),
    )
    .await;

    // Post-state: newname.toml exists with the original fields; oldname is gone.
    assert!(org_config_exists(&tmp, "newname"));
    assert!(!org_config_exists(&tmp, "oldname"), "oldname.toml must be removed");
    assert_eq!(ssh_value(&tmp, "newname", "github").as_deref(), Some("/a"));
    assert_eq!(ws_path_value(&tmp, "newname").as_deref(), Some("/w"));
}

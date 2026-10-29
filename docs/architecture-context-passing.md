# Context Passing Architecture

**Status:** Current Implementation
**Date:** 2026-01-09
**Updated:** 2026-01-11 (OrgConfig propagation)

## Overview

Hyperforge uses a hierarchical routing system where context (org, forge, repo) flows down through nested activations. The CLI path encodes the context:

```
synapse plexus hyperforge org hypermemetic repos diff
        │       │          │      │         │     │
        │       │          │      │         │     └── method
        │       │          │      │         └── child namespace (ReposActivation)
        │       │          │      └── dynamic child (org name)
        │       │          └── child namespace (OrgActivation)
        │       └── service namespace
        └── plexus router
```

---

## Context Flow

**Updated 2026-01-11:** OrgConfig is now passed from parent to child, eliminating redundant GlobalConfig::load() calls.

```
CLI Path: org hypermemetic repos diff

┌─────────────────────────────────────────────────────────────────┐
│ HyperforgeService                                                    │
│   children: [OrgActivation, ForgeActivation, WorkspaceActivation]│
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ get_child("org")
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ OrgActivation                                                    │
│   methods: list(), show(), import(), create()                    │
│   children: dynamic (loaded from config.yaml)                    │
│   loads: GlobalConfig → extracts OrgConfig for child             │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ get_child("hypermemetic")
                              │ → OrgChildRouter::new(paths, "hypermemetic", org_config)
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ OrgChildRouter                                                   │
│   org_name: "hypermemetic"  ← CONTEXT CAPTURED                   │
│   org_config: OrgConfig     ← CONFIG CAPTURED (NEW)              │
│   children: [ReposActivation, SecretsActivation]                 │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ get_child("repos")
                              │ → ReposActivation::new(paths, "hypermemetic", org_config)
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ ReposActivation                                                  │
│   org_name: "hypermemetic"  ← CONTEXT INHERITED                  │
│   org_config: OrgConfig     ← CONFIG INHERITED (NEW)             │
│   methods: diff(), sync(), create(), clone(), clone_all()        │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ diff() method called
                              │ → uses self.org_config directly (no reload!)
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ diff() Implementation (UPDATED)                                  │
│   1. storage = OrgStorage::new(paths, self.org_name)             │
│   2. repos = storage.load_repos()  // ~/.config/hyperforge/orgs/ │
│   3. ~~config = GlobalConfig::load()~~ // ELIMINATED             │
│   4. ~~org_config = config.get_org(&self.org_name)~~ // ELIMINATED│
│   5. forges = self.org_config.forges  // [github, codeberg]      │
└─────────────────────────────────────────────────────────────────┘
```

---

## Key Structs

### OrgChildRouter

Created when navigating to a specific org. Captures org_name **and OrgConfig**, passes both to child activations.

~~**Previous Implementation (org_name only):**~~
```rust
// activations/org/child_router.rs - SUPERSEDED
pub struct OrgChildRouter {
    paths: Arc<HyperforgePaths>,
    org_name: String,  // ← only org_name was captured
}

impl OrgChildRouter {
    pub fn new(paths: Arc<HyperforgePaths>, org_name: String) -> Self {
        Self { paths, org_name }
    }
}
```

**Current Implementation (org_name + OrgConfig):**
```rust
// activations/org/child_router.rs
pub struct OrgChildRouter {
    paths: Arc<HyperforgePaths>,
    org_name: String,
    org_config: OrgConfig,  // ← NEW: full config captured
}

impl OrgChildRouter {
    pub fn new(paths: Arc<HyperforgePaths>, org_name: String, org_config: OrgConfig) -> Self {
        Self { paths, org_name, org_config }
    }

    pub fn org_config(&self) -> &OrgConfig {
        &self.org_config
    }
}

impl ChildRouter for OrgChildRouter {
    async fn get_child(&self, name: &str) -> Option<Box<dyn ChildRouter>> {
        match name {
            "repos" => Some(Box::new(ReposActivation::new(
                self.paths.clone(),
                self.org_name.clone(),
                self.org_config.clone(),  // ← passed down
            ))),
            "secrets" => Some(Box::new(SecretsActivation::new(
                self.paths.clone(),
                self.org_name.clone(),
                // TODO: SecretsActivation should also receive OrgConfig
            ))),
            _ => None,
        }
    }
}
```

### OrgActivation.get_child

Loads config once and extracts OrgConfig to pass down:

```rust
// activations/org/activation.rs
impl ChildRouter for OrgActivation {
    async fn get_child(&self, name: &str) -> Option<Box<dyn ChildRouter>> {
        // Load config and check if org exists
        let config = GlobalConfig::load(&self.paths).await.ok()?;

        // Get the org config if it exists - this will be passed down to children
        let org_config = config.get_org(name)?.clone();

        Some(Box::new(OrgChildRouter::new(
            self.paths.clone(),
            name.to_string(),
            org_config,  // ← passed to child router
        )))
    }
}
```

### ReposActivation

Receives org_name **and OrgConfig** from parent router. Uses stored config for all operations.

~~**Previous Implementation:**~~
```rust
// activations/repos/activation.rs - SUPERSEDED
pub struct ReposActivation {
    paths: Arc<HyperforgePaths>,
    org_name: String,  // ← only org_name
}

// Every method had to reload config:
pub async fn sync(&self, ...) {
    let config = GlobalConfig::load(&paths).await?;  // ← disk I/O
    let org_config = config.get_org(&org_name)?;     // ← lookup
    // ...
}
```

**Current Implementation:**
```rust
// activations/repos/activation.rs
pub struct ReposActivation {
    paths: Arc<HyperforgePaths>,
    org_name: String,
    org_config: OrgConfig,  // ← NEW: config stored
}

impl ReposActivation {
    pub fn new(paths: Arc<HyperforgePaths>, org_name: String, org_config: OrgConfig) -> Self {
        Self { paths, org_name, org_config }
    }
}

// Methods use self.org_config directly:
pub async fn diff(&self, ...) {
    let org_config = self.org_config.clone();  // ← no disk I/O!
    // org_config.forges, org_config.owner, etc. all available
}
```

### RepoChildRouter

Also receives OrgConfig for per-repo operations:

```rust
// activations/repos/repo_router.rs
pub struct RepoChildRouter {
    paths: Arc<HyperforgePaths>,
    org_name: String,
    repo_name: String,
    org_config: OrgConfig,  // ← NEW: config passed from ReposActivation
}
```

---

## How Forges Are Resolved

Forges are not passed through the path. They come from the injected OrgConfig:

~~**Previous (reloaded each method):**~~
```rust
// Inside any ReposActivation method - SUPERSEDED
pub async fn sync(&self, ...) {
    // 1. Load global config - DISK I/O
    let config = GlobalConfig::load(&self.paths).await?;

    // 2. Get org config (contains forge list)
    let org_config = config.get_org(&self.org_name)?;
    // ...
}
```

**Current (uses injected config):**
```rust
// Inside any ReposActivation method
pub async fn sync(&self, ...) {
    // org_config comes from parent - NO DISK I/O for org config
    let org_config = self.org_config.clone();

    // Still need GlobalConfig for workspaces (global-level data)
    let global_config = GlobalConfig::load(&self.paths).await?;
    let workspace_paths = global_config.workspaces.iter()
        .filter(|(_, org)| *org == &self.org_name)
        .collect();

    // Use org_config for org-level data
    let forges = &org_config.forges;  // [github, codeberg]
    let owner = &org_config.owner;     // "hypermemetic"
    let origin = &org_config.origin;   // Codeberg
}
```

### Forge Resolution Priority

```
1. repo_config.forges     (per-repo override)
        ↓ if None
2. self.org_config.forges (org default - from parent context)
```

---

## Storage Paths

Context determines where data is read/written:

```
~/.config/hyperforge/
├── config.yaml                        # GlobalConfig (all orgs)
└── orgs/
    ├── hypermemetic/
    │   ├── repos.yaml                 # ReposConfig for hypermemetic
    │   └── staged-repos.yaml          # Staged changes
    └── juggernautlabs/
        ├── repos.yaml                 # ReposConfig for juggernautlabs
        └── staged-repos.yaml
```

```rust
impl HyperforgePaths {
    pub fn repos_file(&self, org_name: &str) -> PathBuf {
        self.config_dir
            .join("orgs")
            .join(org_name)
            .join("repos.yaml")
    }
}
```

---

## Eliminated Disk I/O

The context passing pattern eliminated 6+ `GlobalConfig::load()` calls:

| Method | Before | After |
|--------|--------|-------|
| `create()` | GlobalConfig::load() for default forges | Uses self.org_config.forges |
| `sync()` | GlobalConfig::load() for org config | Uses self.org_config (still loads for workspaces) |
| `refresh()` | GlobalConfig::load() for forge list | Uses self.org_config directly |
| `diff()` | GlobalConfig::load() for org config | Uses self.org_config directly |
| `converge()` | GlobalConfig::load() for org config | Uses self.org_config directly |
| `clone()` | GlobalConfig::load() for org details | Uses self.org_config directly |
| `clone_all()` | GlobalConfig::load() for org details | Uses self.org_config directly |

**Note:** `sync()` still loads GlobalConfig for workspace bindings, which are at the global level.

---

## Current Limitations

### 1. No Workspace Context

Currently, workspace doesn't inject context. You can't do:
```bash
cd ~/dev/controlflow/hypermemetic
synapse plexus hyperforge repos diff  # ERROR: no org in path
```

You must specify org:
```bash
synapse plexus hyperforge org hypermemetic repos diff
```

### 2. No Forge in Path

Can't target a specific forge:
```bash
# Not supported
synapse plexus hyperforge org hypermemetic forge github repos list
```

Forges are always derived from config, not path.

### 3. ~~Context Not Validated Early~~ (Improved)

~~Org name is captured but not validated until a method runs.~~

**Improved:** OrgConfig is now extracted in `OrgActivation.get_child()`. If org doesn't exist, `get_child()` returns `None` immediately:

```rust
async fn get_child(&self, name: &str) -> Option<Box<dyn ChildRouter>> {
    let config = GlobalConfig::load(&self.paths).await.ok()?;
    let org_config = config.get_org(name)?.clone();  // ← Returns None if not found
    Some(Box::new(OrgChildRouter::new(...)))
}
```

### 4. SecretsActivation Not Yet Updated

SecretsActivation still uses the old pattern (org_name only). Should be updated to receive OrgConfig.

---

## Capabilities Enabled by Context Passing

The parent-to-child context pattern unlocks several enhancement opportunities:

### 1. Credential Caching

**Current:** Each method creates new `KeychainBridge` instances and looks up tokens repeatedly.

**Enabled:** Could add `CredentialCache` to context:
```rust
pub struct OrgContext {
    pub org_config: OrgConfig,
    pub token_cache: Arc<CredentialCache>,  // Pre-loaded tokens
}
```

Benefits:
- Eliminate 8-12 keychain lookups per `converge()` operation
- Pre-validate tokens at org level before repo operations
- Share authenticated state across repos

### 2. Shared HTTP Client

**Current:** Each forge API call creates `reqwest::Client::new()`.

**Enabled:** Could add shared client to context:
```rust
pub struct OrgContext {
    pub org_config: OrgConfig,
    pub http_client: Arc<reqwest::Client>,  // Connection pool reuse
}
```

Benefits:
- Reduce connection overhead 10x in batch operations
- Enable rate limiting coordination
- Share TLS sessions

### 3. Derived State Caching

**Current:** `converge()` reloads repos config 9 times across phases.

**Enabled:** Could cache derived state in context:
```rust
pub struct PhaseCache {
    pub discovered_repos: Arc<HashSet<String>>,
    pub loaded_config: Arc<ReposConfig>,
    pub repo_diffs: Arc<HashMap<String, DiffStatus>>,
}
```

Benefits:
- Reduce file I/O from 9 to 2 calls in `converge()`
- Share discovery results across phases
- Enable phase-to-phase state handoff

### 4. Cross-Repo Parallel Operations

**Current:** Repo operations are sequential with repeated setup.

**Enabled:** With shared context, could parallelize:
```rust
// Pre-load all tokens
let tokens = context.token_cache.acquire_all(&org_config.forges).await;

// Parallel sync across repos
futures::future::join_all(
    repos.iter().map(|r| sync_repo(r, &context))
).await;
```

Benefits:
- Parallelize repo syncs with shared resources
- Batch forge API calls where possible
- Coordinate rate limiting across concurrent operations

### 5. Testing & Dependency Injection

**Current:** Activations create their own dependencies internally.

**Enabled:** Context passing enables clean testing:
```rust
// Production
let context = OrgContext::from_config(org_config);

// Test
let context = OrgContext::mock()
    .with_fake_tokens()
    .with_mock_http_client();
```

Benefits:
- Unit test repo logic without filesystem
- Mock forge APIs for integration tests
- Inject test doubles through context

### 6. Workspace-to-Org Context Flow

**Enabled:** Workspace could inject context into org operations:
```rust
pub struct WorkspaceContext {
    pub workspace_path: PathBuf,
    pub bound_orgs: Vec<(String, OrgConfig)>,  // Pre-resolved
}
```

Benefits:
- Pre-cache workspace → org mappings
- Eliminate `resolve_workspace()` during operations
- Enable multi-org batch operations

---

## Summary

| Level | Context Source | What's Passed |
|-------|---------------|---------------|
| Service | - | Entry point |
| Org | Path segment | `get_child("hypermemetic")` extracts OrgConfig |
| OrgChildRouter | Parent | org_name + OrgConfig |
| Repos | Parent | org_name + OrgConfig |
| RepoChildRouter | Parent | org_name + repo_name + OrgConfig |
| Forge | OrgConfig field | `org_config.forges` |
| Repo | Config lookup | `repos_config.repos.get(name)` |

**Pattern:** Path segments trigger config extraction at the boundary. Parent passes typed configuration to children. Children use stored config directly without reloading.

---

## Relationship to substrate ServiceContext

This pattern mirrors substrate's `ServiceContext` pattern used by Cone and ClaudeCode:

| Substrate | Hyperforge |
|-----------|------------|
| `ServiceContext` trait | `OrgConfig` struct |
| `Weak<Plexus>` | `OrgConfig` (cloned) |
| `inject_parent()` | Constructor parameter |
| `resolve_handle()` | Access org configuration |

The key difference: Substrate's ServiceContext provides _capabilities_ (method calls, handle resolution). Hyperforge's OrgConfig provides _data_ (forge list, owner, SSH keys). Both eliminate redundant lookups by passing context from parent to child.

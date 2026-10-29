# Hyperforge: Declarative Infrastructure-as-Code Service

## Vision

Hyperforge should be a **pure interface to infrastructure-as-code**. Every operation is declarative:
- Local state is a cache of remote truth
- Sync means "make remote match local" OR "make local match remote"
- A successful sync with no changes means IAC is converged
- All mutations flow through the service → IAC pipeline

## Current State

### Architecture Overview

```
HyperforgeService (root)
├── OrgActivation
│   └── OrgChildRouter (per org, dynamic)
│       ├── ReposActivation → RepoChildRouter (per repo)
│       └── SecretsActivation
├── ForgeActivation
│   ├── GitHubRouter
│   └── CodebergRouter
└── WorkspaceActivation
```

### Storage Layer

```
~/.config/hyperforge/
├── config.yaml              # GlobalConfig (orgs, workspaces, defaults)
└── orgs/{org}/
    ├── repos.yaml           # Committed state
    └── staged-repos.yaml    # Pending changes
```

### Data Flow (Current)

```
User Command
    ↓
Service Method (e.g., repos.create)
    ↓
Stage Change → staged-repos.yaml
    ↓
repos.sync --yes
    ↓
Merge staged → committed (repos.yaml)
    ↓
GitRemoteBridge: setup local git remotes
    ↓
PulumiBridge: spawn ./forge subprocess
    ↓
discover.ts: query GitHub/Codeberg APIs, annotate repos.yaml
    ↓
pulumi up: create/update resources on forges
    ↓
Stream PulumiEvents → RepoEvents
```

### What Works

| Feature | Status | Notes |
|---------|--------|-------|
| Org CRUD | ✅ | Creates config, updates SSH config |
| Workspace binding | ✅ | Maps directories to orgs |
| `--auto-create` discovery | ✅ | Scans for git repos on bind |
| Repo staging | ✅ | Stage changes before sync |
| Git remote setup | ✅ | Validates/adds remotes before sync |
| SSH config management | ✅ | Adds Host entries for forge aliases |
| Pulumi preview/up | ✅ | Streams events from subprocess |
| Secrets (keychain) | ✅ | macOS Keychain integration |

---

## Critical Gaps

### Gap 1: Unidirectional Sync Only

**Current**: Local → Remote only

```
repos.yaml → Pulumi → GitHub/Codeberg
```

**Missing**: Remote → Local sync

```
GitHub/Codeberg → discover.ts → repos.yaml → hyperforge storage
                                    ↑
                          This feedback loop doesn't exist
```

**Problem**:
- `discover.ts` queries forges and updates `repos.yaml`
- But hyperforge storage layer never reads this back
- Discovered `_github`, `_gitea_id` annotations are lost
- No way to initialize local state from existing remote repos

### Gap 2: No State Capture After Apply

**Current**: Pulumi creates repos, returns URLs/IDs, but we don't capture them.

```rust
// repos/repo_router.rs:107
// TODO: Call Pulumi bridge

yield RepoEvent::SyncComplete {
    org_name,
    success: true,
    synced_count: 1,  // Hardcoded!
};
```

**Missing**:
- Parse `pulumi stack output` for created URLs
- Store `forge_urls: HashMap<Forge, String>` in RepoDetails
- Track resource IDs for import on subsequent runs

### Gap 3: Protected Repo Deletion Not Enforced

**Current**: `force` parameter is accepted but ignored.

```rust
// repos/activation.rs:394
// TODO: Check if protected and force flag

match storage.stage_deletion(repo_name.clone()).await {
    // Always proceeds, never checks protection
}
```

### Gap 4: Single Repo Sync Not Implemented

**Current**: `_repo_name` parameter is accepted but ignored.

```rust
// repos/activation.rs:194
pub async fn sync(
    &self,
    _repo_name: Option<String>,  // Unused
    dry_run: Option<bool>,
    yes: Option<bool>,
) -> ...
```

### Gap 5: RepoChildRouter::sync Is Stubbed

```rust
// repos/repo_router.rs:107
// TODO: Call Pulumi bridge

yield RepoEvent::SyncComplete {
    org_name,
    success: true,
    synced_count: 1,
};
```

This returns success without doing anything.

---

## Stubbed Implementations & TODOs

| Location | Issue | Priority |
|----------|-------|----------|
| `repos/activation.rs:394` | Protection check not implemented | High |
| `repos/repo_router.rs:107` | Pulumi bridge not called | High |
| `repos/activation.rs:194` | `_repo_name` filter unused | Medium |
| `repos/repo_router.rs:92-93` | `_dry_run`, `_yes` unused | Medium |
| `forge/activation.rs:39-52` | Hardcoded forge list | Low |
| `forge/github.rs:17` | `paths` field unused | Low |
| `forge/codeberg.rs:17` | `paths` field unused | Low |

---

## Target Architecture

### Principle: Service as IAC Interface

```
                    ┌─────────────────────────────────────────┐
                    │         HyperforgeService                   │
                    │  (Pure interface - no direct mutations) │
                    └────────────────┬────────────────────────┘
                                     │
              ┌──────────────────────┼──────────────────────┐
              │                      │                      │
              ▼                      ▼                      ▼
    ┌─────────────────┐   ┌─────────────────┐   ┌─────────────────┐
    │  Query Methods  │   │ Mutation Methods│   │  Sync Methods   │
    │                 │   │                 │   │                 │
    │ org.list()      │   │ org.create()    │   │ repos.sync()    │
    │ repos.list()    │   │ repos.create()  │   │ org.import()    │
    │ workspace.show()│   │ workspace.bind()│   │ repos.refresh() │
    └────────┬────────┘   └────────┬────────┘   └────────┬────────┘
             │                     │                     │
             ▼                     ▼                     ▼
    ┌─────────────────────────────────────────────────────────────┐
    │                   Declarative State Layer                   │
    │                                                             │
    │  ┌─────────────┐    ┌─────────────┐    ┌─────────────┐     │
    │  │ Desired     │    │ Staged      │    │ Committed   │     │
    │  │ (mutations) │───▶│ (pending)   │───▶│ (synced)    │     │
    │  └─────────────┘    └─────────────┘    └─────────────┘     │
    │                                              ▲              │
    │                                              │              │
    │                                    ┌─────────┴─────────┐   │
    │                                    │ Remote State      │   │
    │                                    │ (discovered)      │   │
    │                                    └───────────────────┘   │
    └─────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
    ┌─────────────────────────────────────────────────────────────┐
    │                      Pulumi Layer                           │
    │                                                             │
    │  repos.yaml ──────▶ pulumi up ──────▶ GitHub/Codeberg      │
    │       ▲                                      │              │
    │       │                                      │              │
    │       └──────────── discover.ts ◀────────────┘              │
    │                    (bidirectional)                          │
    └─────────────────────────────────────────────────────────────┘
```

### New Methods Needed

```
org.import(org_name)
  → Discover all repos on configured forges
  → Create hyperforge config from discovered state
  → "Initialize local from remote"

repos.refresh(org_name?)
  → Query forges for current state
  → Update local cache with URLs, IDs, existence flags
  → No mutations, just sync local view

repos.diff(org_name?)
  → Compare local desired state vs remote actual state
  → Return: to_create, to_update, to_delete, in_sync
  → Dry-run without Pulumi

repos.converge(org_name?)
  → Full bidirectional sync
  → 1. Refresh remote state
  → 2. Merge with local desired state
  → 3. Apply via Pulumi
  → 4. Capture outputs back to local
```

### State Model

```yaml
# repos.yaml - Enhanced
owner: hypermemetic
repos:
  substrate:
    # Desired state (user-controlled)
    description: "Core infrastructure"
    visibility: public
    forges: [github, codeberg]
    protected: true

    # Synced state (from Pulumi outputs)
    _synced:
      github:
        url: "https://github.com/hypermemetic/substrate"
        id: "R_kgDOxxxxxxx"
        synced_at: "2025-01-08T12:00:00Z"
      codeberg:
        url: "https://codeberg.org/hypermemetic/substrate"
        id: "12345"
        synced_at: "2025-01-08T12:00:00Z"

    # Discovered state (from forge APIs)
    _discovered:
      github: true
      codeberg: true
      last_refresh: "2025-01-08T12:00:00Z"
```

### Convergence Algorithm

```
CONVERGE(org):
  1. REFRESH: Query forges, update _discovered fields

  2. DIFF:
     for each repo in local:
       if not _discovered.{forge}: mark CREATE
       if _discovered but config differs: mark UPDATE
     for each repo on remote not in local:
       if auto_import: add to local
       else: warn "untracked remote repo"

  3. APPLY (if not dry_run):
     Pulumi up with current repos.yaml

  4. CAPTURE:
     Parse Pulumi outputs
     Update _synced fields with URLs, IDs, timestamps

  5. VERIFY:
     Re-run DIFF
     If empty: "Converged"
     If not: "Drift detected" (should not happen)
```

---

## Implementation Roadmap

### Phase 1: Fix Stubs (Current Gaps)

1. **RepoChildRouter::sync** - Wire up Pulumi bridge
2. **Protection checks** - Enforce `protected` flag in `remove()`
3. **Single repo sync** - Use `repo_name` filter parameter

### Phase 2: Bidirectional State

1. **repos.refresh()** - Query forges, update `_discovered`
2. **Capture Pulumi outputs** - Parse stack output, update `_synced`
3. **Enhanced ReposConfig** - Add `_synced`, `_discovered` fields

### Phase 3: Full Convergence

1. **org.import()** - Initialize from remote
2. **repos.diff()** - Compare local vs remote
3. **repos.converge()** - Full bidirectional sync
4. **Drift detection** - Warn on untracked remote changes

### Phase 4: Advanced Features

1. **Watch mode** - Continuous sync with file watching
2. **Webhook receiver** - React to forge events
3. **Multi-org operations** - Bulk sync across orgs
4. **Migration tooling** - Import from other systems

---

## Key Files Reference

| File | Purpose | Status |
|------|---------|--------|
| `service.rs` | Root service, namespace routing | ✅ Complete |
| `activations/org/activation.rs` | Org CRUD | ✅ Complete |
| `activations/repos/activation.rs` | Repo staging, sync | ⚠️ Gaps |
| `activations/repos/repo_router.rs` | Per-repo operations | ❌ Stubbed |
| `bridge/pulumi.rs` | Pulumi subprocess | ✅ Complete |
| `bridge/git_remote.rs` | Git remote setup | ✅ Complete |
| `bridge/ssh_config.rs` | SSH config management | ✅ Complete |
| `storage/org_storage.rs` | Repos config I/O | ✅ Complete |
| `~/.hypermemetic-infra/projects/forge-pulumi/` | Pulumi program | ✅ External |

---

## Success Criteria

A successful implementation means:

1. **Initialize**: `org.import myorg` creates local config from GitHub/Codeberg
2. **Verify**: `repos.sync --dry-run` shows "0 changes" (converged)
3. **Mutate**: `repos.create new-repo` stages a change
4. **Apply**: `repos.sync --yes` creates on forges
5. **Capture**: Local state now has `_synced.github.url`, etc.
6. **Idempotent**: Re-running sync shows "0 changes"
7. **Drift**: Manual forge changes detected on next `repos.refresh`

The service becomes a pure declarative interface where:
- All state is derived from the IAC layer
- All mutations flow through Pulumi
- Local cache is always reconcilable with remote truth

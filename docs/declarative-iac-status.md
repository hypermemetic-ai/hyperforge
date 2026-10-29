# Hyperforge Declarative IAC - Implementation Status

## Overview

This document captures the current state of implementing declarative infrastructure-as-code for hyperforge, enabling "local as source of truth" repo management across GitHub and Codeberg.

## Architecture Summary

The system uses a service-based architecture where:
1. **Local `repos.yaml`** = desired state (what repos should exist)
2. **`_synced` field** = last applied state (what Pulumi created)
3. **`repos.diff`** = compares desired vs synced
4. **`repos.sync`** = applies desired state via Pulumi, captures outputs to `_synced`

### Key Files

| File | Purpose |
|------|---------|
| `~/.config/hyperforge/config.yaml` | Global config with orgs, forges, secrets provider |
| `~/.config/hyperforge/orgs/<org>/repos.yaml` | Repo definitions with `_synced` state |
| `~/.config/hyperforge/orgs/<org>/staged-repos.yaml` | Pending changes before sync |
| `~/.hypermemetic-infra/projects/forge-pulumi/` | Pulumi project for creating repos |

### New repos.yaml Format

```yaml
owner: hypermemetic
repos:
  substrate:
    description: "Core infrastructure"
    visibility: public
    forges: [github, codeberg]
    protected: false
    _delete: false
    _synced:
      github:
        url: "https://github.com/hypermemetic/substrate"
        id: null
        synced_at: "2026-01-08T..."
      codeberg:
        url: "https://codeberg.org/hypermemetic/substrate"
        id: null
        synced_at: "2026-01-08T..."
```

## What's Working

### 1. Service Infrastructure
- All SERVICE tickets (SERVICE-14 through SERVICE-22) implemented
- Dynamic org routing: `synapse plexus hyperforge org <org_name> repos <method>`
- Org children exposed in schema via `plugin_children()` loading from config

### 2. Commands Working
```bash
# List orgs
synapse plexus hyperforge org list

# Import repos from forges (creates new-format repos.yaml with _synced)
synapse plexus hyperforge org import --org_name hypermemetic --include_private true

# Show diff (compares desired vs synced, includes staged repos)
synapse plexus hyperforge org hypermemetic repos diff

# Stage a new repo
synapse plexus hyperforge org hypermemetic repos create --repo_name <name> --description <desc> --visibility public

# Sync to forges (runs Pulumi)
synapse plexus hyperforge org hypermemetic repos sync --yes true
```

### 3. Fixes Applied

#### Path Fix (`storage/paths.rs`)
Changed from `directories::BaseDirs` (which uses `~/Library/Application Support/` on macOS) to hardcoded `~/.config/hyperforge/`:
```rust
let home = std::env::var("HOME").expect("HOME not set");
let config_dir = PathBuf::from(home).join(".config").join("hyperforge");
```

#### Keychain Format Fix (`bridge/keychain.rs`)
Changed separator from dots to colons to match existing hyperforge CLI:
```rust
// Old (wrong): hyperforge.hypermemetic.github-token
// New (correct): hyperforge:hypermemetic:github-token
service_prefix: format!("hyperforge:{}", org_name)
```

Also uses `$USER` as account name instead of "hyperforge".

#### Pulumi Passphrase (`bridge/pulumi.rs`)
Added `PULUMI_CONFIG_PASSPHRASE=""` to all Pulumi command invocations:
- `select_stack()` - both select and init
- `preview()`
- `up()`
- `get_outputs()`

#### Forge Script Mode (`bridge/pulumi.rs`)
Changed from `./forge sync` (which runs discover.ts and overwrites repos.yaml) to `./forge up` (just runs Pulumi):
```rust
cmd.arg("up") // Just run pulumi up, skip discover.ts
```

#### Diff Includes Staged (`activations/repos/activation.rs`)
Updated `diff()` to merge staged repos before comparing:
```rust
if let Ok(staged) = storage.load_staged().await {
    for (name, config) in staged.repos {
        repos.repos.insert(name, config);
    }
}
```

## Status: ✅ FULLY WORKING

**Verified 2026-01-08**: All fixes applied and tested. The full declarative IAC workflow is operational:

1. `org import` → populates repos.yaml with `_synced` state from existing forge repos
2. `repos create` → stages new repo to `staged-repos.yaml`
3. `repos diff` → compares desired vs synced, includes staged repos
4. `repos sync` → runs Pulumi, creates repos on forges, captures URLs back to `_synced`

### Test Results

```bash
# Created test repo
synapse plexus hyperforge org hypermemetic repos create --repo_name test-output-capture --description "Testing output capture fix" --visibility public

# Diff showed to_create: 1
synapse plexus hyperforge org hypermemetic repos diff

# Sync created repo on both forges
synapse plexus hyperforge org hypermemetic repos sync --yes true

# Diff now shows in_sync: 10, to_create: 0
synapse plexus hyperforge org hypermemetic repos diff

# Verified _synced populated in repos.yaml:
# _synced:
#   github:
#     url: https://github.com/hypermemetic/test-output-capture
#     synced_at: 2026-01-08T10:07:50.325551Z
#   codeberg:
#     url: https://codeberg.org/hypermemetic/test-output-capture
#     synced_at: 2026-01-08T10:07:50.327451Z
```

## Resolved Issues

### Output Capture Fix (`bridge/pulumi.rs`)

**Problem**: After `repos.sync` ran Pulumi successfully, the `_synced` state was not being updated.

**Root Cause**: `get_outputs()` parsed wrong JSON format.

**Fix Applied**:
```rust
// Changed from:
json.get("repos") // wrong key
data.get("github").and_then(|g| g.get("url")) // nested objects

// To:
json.get("repositories") // correct key
data.get("github").and_then(|v| v.as_str()) // direct string values
```

**Also Added**: Calls to `storage.update_synced()` after capturing outputs (was only emitting events, not persisting).

## Code Locations

### Service Implementation
- `hyperforge/src/activations/org/activation.rs` - OrgActivation with import(), list(), show()
- `hyperforge/src/activations/org/child_router.rs` - OrgChildRouter for org-specific routes
- `hyperforge/src/activations/repos/activation.rs` - ReposActivation with sync(), diff(), create(), etc.
- `hyperforge/src/bridge/pulumi.rs` - PulumiBridge for Pulumi operations
- `hyperforge/src/bridge/keychain.rs` - KeychainBridge for secrets
- `hyperforge/src/storage/org_storage.rs` - OrgStorage with update_synced(), merge_staged()
- `hyperforge/src/types/repo.rs` - RepoConfig, SyncedState, ForgeSyncedState types

### Events
- `hyperforge/src/events.rs` - All event types (RepoEvent, OrgEvent, PulumiEvent)

### Testing Documentation
- `hyperforge/docs/testing-declarative-iac.md` - Full test scenarios

## Test Repos Created

These repos were created during testing and exist on forges:
- `hypermemetic/test-local-source` - GitHub + Codeberg
- `hypermemetic/test-cycle-2` - GitHub + Codeberg
- `hypermemetic/test-output-capture` - GitHub + Codeberg (verified output capture working)

## Cleanup Commands

```bash
# Delete test repos from forges (GitHub)
gh repo delete hypermemetic/test-local-source --yes
gh repo delete hypermemetic/test-cycle-2 --yes
gh repo delete hypermemetic/test-output-capture --yes

# Delete from Codeberg (no CLI, use web UI or API)
# https://codeberg.org/hypermemetic/test-local-source/settings
# https://codeberg.org/hypermemetic/test-cycle-2/settings
# https://codeberg.org/hypermemetic/test-output-capture/settings

# Reset local config
rm ~/.config/hyperforge/orgs/hypermemetic/repos.yaml
rm ~/.config/hyperforge/orgs/hypermemetic/staged-repos.yaml
```

## Next Steps

1. ~~**Verify output capture fix**~~ - ✅ DONE (2026-01-08)
2. **Test converge workflow** - `repos.converge --yes true` should run full cycle
3. **Add SERVICE-13** - Migration from old hyperforge CLI (not yet started)
4. **Clean up test repos** - Delete test-local-source, test-cycle-2, test-output-capture from forges
5. **Delete workflow** - Test marking repos with `_delete: true` and syncing

## Command Syntax Reference

Path segments are **space-separated**, not dot-separated:
```bash
# Correct
synapse plexus hyperforge org hypermemetic repos diff

# Wrong
synapse plexus hyperforge.org.hypermemetic.repos.diff
```

## Dependencies

- Pulumi CLI installed and configured
- `~/.hypermemetic-infra/projects/forge-pulumi/` exists with `./forge` script
- Tokens in keychain: `hyperforge:hypermemetic:github-token`, `hyperforge:hypermemetic:codeberg-token`
- substrate server running (auto-restarts on binary change)

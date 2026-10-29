# Hyperforge Service Testing Status

**Last Updated:** 2026-01-08

This document tracks what has been tested and verified in the hyperforge service implementation.

## SSH URL Pattern

**Pattern:** `git@<forge>-<org_name>:<owner>/<repo>.git`

Examples:
- `git@github-hypermemetic:hypermemetic/substrate.git`
- `git@codeberg-hypermemetic:hypermemetic/substrate.git`
- `git@github-juggernautlabs:juggernautlabs/cllient.git`

**Abstraction Location:**
- `types/org.rs` - `Org::ssh_host()`, `Org::ssh_url()`, `Org::origin_url()`
- `storage/config.rs` - `OrgConfig::ssh_host()`, `OrgConfig::ssh_url()`, `OrgConfig::origin_url()`
- `bridge/git_remote.rs` - `GitRemoteBridge::build_remote_url()`

---

## Tested Features

### Organization Management

| Feature | Command | Status | Notes |
|---------|---------|--------|-------|
| List orgs | `org list` | ✅ Tested | Returns configured orgs from config.yaml |
| Show org | `org show --org_name <org>` | ✅ Tested | Shows org details |
| Import repos | `org import --org_name <org>` | ✅ Tested | Imports from GitHub + Codeberg |
| Import with private | `org import --include_private true` | ✅ Tested | Includes private repos |

### Repository Management

| Feature | Command | Status | Notes |
|---------|---------|--------|-------|
| List repos | `org <org> repos list` | ✅ Tested | Lists repos from repos.yaml |
| Diff | `org <org> repos diff` | ✅ Tested | Shows desired vs synced state |
| Create/stage | `org <org> repos create` | ✅ Tested | Stages new repo to staged-repos.yaml |
| Sync | `org <org> repos sync --yes true` | ✅ Tested | Creates repos via Pulumi |
| Output capture | After sync | ✅ Tested | `_synced` field populated with URLs |
| Clone single | `org <org> repos clone --repo_name <repo>` | ✅ Tested | Clones with all remotes configured |
| Clone all | `org <org> repos clone_all --target <path>` | ✅ Tested | Clones all org repos |

### Tested Workflows

#### 1. Full Create Cycle (hypermemetic)
```bash
# Stage new repo
synapse plexus hyperforge org hypermemetic repos create \
  --repo_name test-output-capture \
  --description "Testing output capture fix" \
  --visibility public

# Verify staged
synapse plexus hyperforge org hypermemetic repos diff
# Shows: to_create: 1

# Sync to forges
synapse plexus hyperforge org hypermemetic repos sync --yes true

# Verify synced
synapse plexus hyperforge org hypermemetic repos diff
# Shows: in_sync: 10, to_create: 0

# Verify _synced in repos.yaml contains URLs
```
**Result:** ✅ Working

#### 2. Clone Single Repo (hypermemetic)
```bash
synapse plexus hyperforge org hypermemetic repos clone \
  --repo_name substrate \
  --target /tmp/test-clone-substrate

# Verifies:
# - Clones from origin (codeberg)
# - Adds github remote
# - Fetches all
# - Reports sync status
```
**Result:** ✅ Working

#### 3. Clone All Repos (juggernautlabs)
```bash
synapse plexus hyperforge org juggernautlabs repos clone_all \
  --target ~/repos/juggernautlabs

# Verifies:
# - Skips existing repos
# - Clones missing repos
# - Configures remotes correctly
```
**Result:** ✅ Working

#### 4. Import from Forges (hypermemetic)
```bash
synapse plexus hyperforge org import \
  --org_name hypermemetic \
  --include_private true

# Verifies:
# - Queries GitHub and Codeberg APIs
# - Creates repos.yaml with _synced state pre-populated
# - Skips already-tracked repos
```
**Result:** ✅ Working

---

## Untested Features

### Organization Management

| Feature | Command | Status | Notes |
|---------|---------|--------|-------|
| Create org | `org create` | ❓ Untested | Creates new org in config.yaml |
| Remove org | `org remove` | ❓ Untested | Removes org from config.yaml |
| SSH config update | After org create | ❓ Untested | Should update ~/.ssh/config |

### Repository Management

| Feature | Command | Status | Notes |
|---------|---------|--------|-------|
| Remove/delete | `org <org> repos remove` | ❓ Untested | Marks repo for deletion |
| Sync delete | `repos sync` with `_delete: true` | ❓ Untested | Should delete from forges |
| Refresh | `org <org> repos refresh` | ❓ Untested | Query forges for current state |
| Converge | `org <org> repos converge` | ❓ Untested | Full bidirectional sync |
| List staged | `org <org> repos list --staged true` | ❓ Untested | Show staged repos |
| Sync preview | `repos sync --dry_run true` | ❓ Untested | Preview without applying |

### Secrets Management

| Feature | Command | Status | Notes |
|---------|---------|--------|-------|
| List secrets | `org <org> secrets list` | ❓ Untested | List keychain keys |
| Get secret | `org <org> secrets get` | ❓ Untested | Retrieve from keychain |
| Set secret | `org <org> secrets set` | ❓ Untested | Store in keychain |

### Workspace Management

| Feature | Command | Status | Notes |
|---------|---------|--------|-------|
| List workspaces | `workspace list` | ❓ Untested | Show workspace bindings |
| Bind workspace | `workspace bind` | ❓ Untested | Bind path to org |
| Resolve workspace | `workspace resolve` | ❓ Untested | Find org for current path |

### Forge Direct Operations

| Feature | Command | Status | Notes |
|---------|---------|--------|-------|
| List forge repos | `forge <forge> repos` | ❓ Untested | Query forge API directly |
| Auth status | `forge <forge> auth` | ❓ Untested | Check authentication |

---

## Known Issues

### Resolved

1. **Path resolution on macOS** - Fixed in `storage/paths.rs`
   - Was using `~/Library/Application Support/`, now uses `~/.config/hyperforge/`

2. **Keychain format** - Fixed in `bridge/keychain.rs`
   - Changed from dots to colons: `hyperforge:org:key`

3. **Pulumi passphrase** - Fixed in `bridge/pulumi.rs`
   - Added `PULUMI_CONFIG_PASSPHRASE=""` to all commands

4. **discover.ts overwriting repos.yaml** - Fixed in `bridge/pulumi.rs`
   - Changed from `./forge sync` to `./forge up`

5. **Output capture parsing** - Fixed in `bridge/pulumi.rs`
   - Fixed JSON key from `repos` to `repositories`
   - Fixed value parsing from nested objects to direct strings

6. **SSH URL pattern inconsistency** - Fixed
   - Standardized on `<forge>-<org_name>` pattern
   - Abstracted into `OrgConfig::ssh_url()` methods

### Open

1. **GitLab support** - Not implemented
   - Import, sync, clone all return "not implemented" for GitLab

2. **Pagination** - Not implemented
   - API queries limited to first 100 repos

3. **Rate limiting** - Not handled
   - No retry logic for API rate limits

---

## Test Repos Created

These repos were created during testing and exist on forges:

| Repo | Forges | Purpose | Cleanup? |
|------|--------|---------|----------|
| `hypermemetic/test-local-source` | GitHub, Codeberg | Testing local as source | Yes |
| `hypermemetic/test-cycle-2` | GitHub, Codeberg | Testing full cycle | Yes |
| `hypermemetic/test-output-capture` | GitHub, Codeberg | Testing output capture | Yes |

### Cleanup Commands

```bash
# GitHub
gh repo delete hypermemetic/test-local-source --yes
gh repo delete hypermemetic/test-cycle-2 --yes
gh repo delete hypermemetic/test-output-capture --yes

# Codeberg (manual via web UI)
# https://codeberg.org/hypermemetic/test-local-source/settings
# https://codeberg.org/hypermemetic/test-cycle-2/settings
# https://codeberg.org/hypermemetic/test-output-capture/settings

# Local cleanup
rm -rf /tmp/test-clone-substrate
```

---

## Multi-Org Testing

| Org | Forges | Import | Clone | Sync | Notes |
|-----|--------|--------|-------|------|-------|
| hypermemetic | GitHub, Codeberg | ✅ | ✅ | ✅ | Primary test org |
| juggernautlabs | GitHub only | ✅ | ✅ | ❓ | Single-forge org |

---

## Command Syntax Reference

All commands use space-separated paths:

```bash
# Correct
synapse plexus hyperforge org hypermemetic repos diff

# Wrong (dots)
synapse plexus hyperforge.org.hypermemetic.repos.diff
```

---

## Dependencies

- Pulumi CLI installed
- `~/.hypermemetic-infra/projects/forge-pulumi/` with `./forge` script
- Tokens in keychain: `hyperforge:<org>:github-token`, `hyperforge:<org>:codeberg-token`
- SSH config with Host entries: `github-<org>`, `codeberg-<org>`
- substrate server running (auto-restarts on binary change)

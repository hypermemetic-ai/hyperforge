# LFORGE2-1: Hyperforge Redesign - Epic Overview

**Status**: Planning
**Epic**: LFORGE2 (Repo-Local Forge Management)
**Implementation**: Fresh start in hyperforge worktree, delete all source
**Goal**: Redesign hyperforge as workspace-centric, git-native, repo-local multi-forge management

---

## Introduction: What is Hyperforge?

Hyperforge removes the friction from managing repositories across multiple git forges (GitHub, Codeberg, GitLab). Instead of manually configuring remotes, syncing branches, and publishing packages, hyperforge automates the boring parts while staying out of your way.

### The Key Insight

**Git already manages repositories. Hyperforge should leverage that, not duplicate it.**

- Git remotes = which forges you're mirroring to
- Git branch tracking = sync status
- Git config = repository metadata
- `.hyperforge/` = which forges to maintain, how to publish

### Common Use Cases

#### Use Case 1: Start a new project, mirror to multiple forges

```bash
# Create a new project
mkdir my-tool && cd my-tool

# Initialize with hyperforge
hyperforge init --path . --forges github,codeberg

# What happened:
# - git init (if needed)
# - Created .hyperforge/config.toml
# - Added remotes: origin (codeberg), github
# - Ready to work

# Normal git workflow
git add .
git commit -m "Initial commit"

# Push to all forges
hyperforge push --path .
# → git push origin main
# → git push github main
```

#### Use Case 2: Adopt an existing repo

```bash
# You have an existing repo on GitHub
cd ~/projects/existing-project

# Add hyperforge management
hyperforge init --path . --forges github,codeberg

# It detects existing GitHub remote, adds Codeberg
git remote -v
# origin    git@github.com:user/existing-project.git
# codeberg  git@codeberg.org:user/existing-project.git (just added)

# Push to sync
hyperforge push --path .
```

#### Use Case 3: Workspace operations (the "hyper" concept)

```bash
# You have multiple repos in a directory
~/projects/
  ├── tool-1/.hyperforge/
  ├── tool-2/.hyperforge/
  ├── lib-core/.hyperforge/
  └── other-repo/  (no .hyperforge, not managed)

# From the parent directory, operate on ALL managed repos:
cd ~/projects

# List all hyperforge-managed repos
hyperforge workspace list --path .
# → tool-1 [github, codeberg] ✓
# → tool-2 [github, codeberg] ✓
# → lib-core [github] ✓

# Push all managed repos ("hyper push")
hyperforge workspace push --path .
# → Pushes tool-1 to github, codeberg
# → Pushes tool-2 to github, codeberg
# → Pushes lib-core to github
# → Skips other-repo (not managed)

# Publish all packages with version bump
hyperforge workspace publish --path . --bump patch
# → Bumps lib-core (detects Cargo.toml)
# → Bumps tool-1 (depends on lib-core, respects order)
# → Bumps tool-2
```

#### Use Case 4: Package publishing with dependency order

```bash
# You have a workspace with interdependent crates:
~/projects/
  ├── core/          # lib, published to crates.io
  ├── cli/           # binary, depends on ../core
  └── plugins/       # plugins, depend on ../core

# Publish all with correct dependency order
cd ~/projects
hyperforge workspace publish --path . --bump minor --deps

# What happens:
# 1. Detects core has no local dependencies → publish first (1.2.0 → 1.3.0)
# 2. Detects cli depends on core → wait for core, then publish
# 3. Detects plugins depend on core → wait for core, then publish
# 4. Updates Cargo.toml in cli and plugins to use core = "1.3.0"
# 5. Commits version bumps
# 6. Pushes all repos
```

#### Use Case 5: The original "hyper push" from ~/.hypermemetic-infra

```bash
# The ~/.hypermemetic-infra workspace contains many repos:
~/.hypermemetic-infra/
  ├── hyperforge/.hyperforge/
  ├── substrate/.hyperforge/
  ├── synapse/.hyperforge/
  ├── plexus-core/.hyperforge/
  └── ... etc

# One command to push everything:
cd ~/.hypermemetic-infra
hyperforge workspace push --path .

# This is the "hyper push" - push all managed repos in a workspace
```

---

## Architecture Overview

### Repo-Local Configuration

Each repository managed by hyperforge has a `.hyperforge/` directory:

```
my-repo/
  ├── .git/
  ├── .hyperforge/
  │   └── config.toml          # This repo's hyperforge config
  ├── Cargo.toml               # Package metadata (detected)
  └── src/
```

**`.hyperforge/config.toml`:**
```toml
# Which forges to mirror to
forges = ["github", "codeberg"]

# Optional: explicit remote configuration
[remotes.origin]
forge = "codeberg"
ssh_key = "hypermemetic.proton.me"

[remotes.github]
forge = "github"
ssh_key = "hypermemetic.proton.me"

# Optional: package metadata (auto-detected if not specified)
[package]
name = "my-crate"
registry = "crates.io"
```

**Why repo-local?**
- Each repo independently configured
- No global state to get out of sync
- Repos are portable (copy `.hyperforge/`, it works)
- Multiple unrelated repos can coexist in same directory

### Workspaces are Emergent

A "workspace" is any directory containing repos with `.hyperforge/` config. There's no workspace config file - hyperforge discovers managed repos by walking the directory tree.

```bash
# This is a workspace (3 managed repos):
~/projects/
  ├── repo-1/.hyperforge/
  ├── repo-2/.hyperforge/
  └── repo-3/.hyperforge/

# This is also a workspace (2 managed, 1 not):
~/code/
  ├── managed-1/.hyperforge/
  ├── managed-2/.hyperforge/
  └── unmanaged/  (no .hyperforge)
```

**Discovery rules:**
- Start from `--path`
- Recursively find all `.hyperforge/` directories
- Stop recursion at `.git/` boundaries (don't descend into submodules)
- Ignore `.hyperforge/` inside `.git/`

### Git as Source of Truth

Hyperforge reads git state to understand sync status:

```bash
# Git tells us which forges are configured
git remote -v
# origin    git@codeberg-key:user/repo.git
# github    git@github-key:user/repo.git

# Git tells us sync status
git branch -vv
# * main abc1234 [origin/main: ahead 2] Latest changes

# Hyperforge uses this information:
hyperforge status --path .
# ✓ origin (codeberg): up to date
# ↑ github: 2 commits ahead (need to push)
```

**Hyperforge doesn't duplicate this state:**
- Reads from `git remote -v` to see configured remotes
- Reads from `git status` and `git branch -vv` for sync status
- Writes to git remotes via `git remote add/remove`
- Lets git track everything else

**Reconciliation:**
```bash
# .hyperforge/config.toml says:
forges = ["github", "codeberg", "gitlab"]

# But git remotes are:
# origin    codeberg
# github    github

# hyperforge sync --path . will:
# - Add missing: gitlab
# - Keep existing: origin, github
# - Remove extras: none
```

### Auth Module (Separate Plugin)

Authentication is handled by a separate auth plugin, not part of hyperforge:

```
┌─────────────┐
│  auth-service   │  Runs at startup, loads all available keys
└──────┬──────┘
       │ Issues access token to other services
       ↓
┌─────────────┐
│ hyperforge  │  Uses token to request specific keys when needed
└──────┬──────┘
       │ "Need: github-token for user@github.com"
       ↓
┌─────────────┐
│  auth-service   │  Approval queue (with timeout)
└─────────────┘  User approves, denies, or times out
```

**Auth flow:**
1. Hyperforge needs to access GitHub
2. Requests "github-token" from auth-service via access token
3. Auth-service either:
   - Auto-approves (if policy allows)
   - Prompts user for approval
   - Times out after N seconds
4. Returns token or error to hyperforge
5. Hyperforge retries or fails appropriately

**Why separate?**
- Auth is complex, security-sensitive
- Many plugins need auth (not just hyperforge)
- Centralized approval UI
- Hyperforge stays focused on forge management

---

## Core Operations

### Single-Repo Commands

All single-repo commands require `--path` (synapse limitation - can't use implicit CWD):

| Command | Purpose | Example |
|---------|---------|---------|
| `init` | Initialize hyperforge for this repo | `hyperforge init --path . --forges github,codeberg` |
| `status` | Show sync status (which remotes ahead/behind) | `hyperforge status --path .` |
| `push` | Push to all configured forges | `hyperforge push --path .` |
| `pull` | Pull from origin | `hyperforge pull --path .` |
| `sync` | Reconcile remotes (add missing, remove extras) | `hyperforge sync --path .` |
| `publish` | Publish package with version bump | `hyperforge publish --path . --bump patch` |
| `add` | Add a forge to this repo | `hyperforge add --path . --forge gitlab` |
| `remove` | Remove a forge from this repo | `hyperforge remove --path . --forge gitlab` |

### Workspace Commands

Workspace commands operate on all repos with `.hyperforge/` under `--path`:

| Command | Purpose | Example |
|---------|---------|---------|
| `workspace list` | List all managed repos | `hyperforge workspace list --path .` |
| `workspace status` | Status for all repos | `hyperforge workspace status --path .` |
| `workspace push` | Push all repos ("hyper push") | `hyperforge workspace push --path .` |
| `workspace pull` | Pull all repos | `hyperforge workspace pull --path .` |
| `workspace sync` | Sync all repos | `hyperforge workspace sync --path .` |
| `workspace publish` | Publish all packages | `hyperforge workspace publish --path . --bump patch` |

---

## Design Principles

### 1. Git is Source of Truth
- **Don't duplicate git's state management**
- Read from git to understand state (`git remote`, `git status`, `git branch`)
- Write to git remotes for configuration (`git remote add/remove`)
- Let git track sync status (ahead/behind/up-to-date)
- Never maintain a separate "synced state" file

### 2. Workspace is Emergent
- **No workspace config file**
- Discovered by walking directory tree
- Each repo independently configured via `.hyperforge/config.toml`
- Workspace operations are just aggregations over discovered repos
- No workspace-level state to get out of sync

### 3. Repo-Local Configuration
- Each repo has `.hyperforge/config.toml`
- No global state beyond auth
- Repos are portable (copy `.hyperforge/`, works anywhere)
- Multiple unrelated repos can coexist in same directory tree

### 4. Auth is Separate
- Hyperforge requests keys via auth plugin
- Auth plugin handles approval/storage/policy
- Clean separation of concerns
- Timeout prevents hanging forever

### 5. Convention over Configuration
- **Smart defaults**:
  - Detect package type from manifest files
  - Infer remote URLs from forge name + repo name
  - Use SSH config aliases (`github-keyname`, `codeberg-keyname`)
- **Explicit when needed**: `.hyperforge/config.toml` for overrides
- **Fail loudly on ambiguity**: If can't infer, require explicit config

### 6. Operations are Idempotent
- Running `hyperforge sync --path .` twice does nothing the second time
- Running `hyperforge push --path .` on up-to-date repo is no-op
- Init detects existing config and reconciles instead of failing

---

## Epic Breakdown

This epic is broken into tickets that can be worked on in parallel after foundational work completes.

### Phase 1: Foundation (Sequential)

**LFORGE2-2: Core types and configuration**
- Define `.hyperforge/config.toml` schema
- `RepoConfig`, `ForgeConfig`, `RemoteConfig` types
- Config loading/saving with `serde` + `toml`
- Config validation (required fields, valid forge names)
- `blocked_by: []`
- `unlocks: [LFORGE2-3, LFORGE2-4, LFORGE2-5, LFORGE2-11]`

**LFORGE2-3: Git integration layer**
- Git command execution (`git remote`, `git status`, `git push`, etc.)
- Parse git output (remotes, branch tracking, ahead/behind)
- Git remote manipulation (add, remove, set-url)
- Error handling for git failures
- `blocked_by: [LFORGE2-2]`
- `unlocks: [LFORGE2-4, LFORGE2-5, LFORGE2-6, LFORGE2-7]`

### Phase 2: Single-Repo Operations (Parallel after Phase 1)

**LFORGE2-4: Init command**
- `hyperforge init --path . --forges X`
- Git init if needed
- Create `.hyperforge/config.toml`
- Configure git remotes based on forges + SSH key config
- Handle existing repos (reconcile, don't fail)
- `blocked_by: [LFORGE2-2, LFORGE2-3]`
- `unlocks: [LFORGE2-6, LFORGE2-7, LFORGE2-9]`

**LFORGE2-5: Sync command**
- `hyperforge sync --path .`
- Read `.hyperforge/config.toml` (desired forges)
- Read `git remote -v` (actual remotes)
- Reconcile: add missing, remove extras
- Infer remote URLs from forge name + repo identity
- `blocked_by: [LFORGE2-2, LFORGE2-3]`
- `unlocks: [LFORGE2-6]`

**LFORGE2-6: Status command**
- `hyperforge status --path .`
- Read git branch status (`git status`, `git branch -vv`)
- Compare remotes to config
- Show ahead/behind per forge
- Pretty formatting (✓ up-to-date, ↑ ahead, ↓ behind, ✗ error)
- `blocked_by: [LFORGE2-3, LFORGE2-4, LFORGE2-5]`
- `unlocks: []`

**LFORGE2-7: Push command**
- `hyperforge push --path .`
- Push current branch to all configured forges
- Respect git branch tracking
- Report success/failure per forge
- Handle partial failures (some succeed, some fail)
- `blocked_by: [LFORGE2-3, LFORGE2-4]`
- `unlocks: [LFORGE2-9]`

**LFORGE2-8: Pull command**
- `hyperforge pull --path .`
- Pull from origin (determined by git config)
- Detect conflicts and report
- Simple wrapper around `git pull`
- `blocked_by: [LFORGE2-3]`
- `unlocks: []`

### Phase 3: Workspace Discovery (After Phase 2)

**LFORGE2-9: Workspace discovery**
- Walk directory tree from `--path`
- Find all `.hyperforge/config.toml` files
- Stop recursion at `.git/` boundaries
- Build list of managed repos
- Handle errors (invalid config, missing .git, etc.)
- `blocked_by: [LFORGE2-4, LFORGE2-7]`
- `unlocks: [LFORGE2-10, LFORGE2-13]`

### Phase 4: Workspace Operations (Parallel after Phase 3)

**LFORGE2-10: Workspace push**
- `hyperforge workspace push --path .`
- Discover all managed repos via LFORGE2-9
- Push each repo to its configured forges
- Aggregate results (success/failure per repo)
- Continue on individual failures (don't stop entire operation)
- `blocked_by: [LFORGE2-9]`
- `unlocks: []`

**LFORGE2-11: Workspace status**
- `hyperforge workspace status --path .`
- Discover all managed repos
- Status for each repo (ahead/behind per forge)
- Aggregate view (table format)
- Summary: X repos up-to-date, Y need push, Z have errors
- `blocked_by: [LFORGE2-9]`
- `unlocks: []`

**LFORGE2-12: Workspace list**
- `hyperforge workspace list --path .`
- Discover all managed repos
- List repo names, configured forges, status
- Simple table output
- `blocked_by: [LFORGE2-9]`
- `unlocks: []`

### Phase 5: Package Publishing (Parallel, independent path)

**LFORGE2-13: Package detection**
- Detect `Cargo.toml`, `package.json`, `mix.exs`, `pyproject.toml`, `setup.py`, `*.cabal`
- Extract package name and version
- Parse local path dependencies (for workspace publishing)
- Build dependency graph for workspace
- `blocked_by: [LFORGE2-2]`
- `unlocks: [LFORGE2-14, LFORGE2-15]`

**LFORGE2-14: Single-repo publish**
- `hyperforge publish --path . --bump patch`
- Version bump (patch/minor/major) following semver
- Update manifest file with new version
- Commit version change
- Publish to registry (cargo publish, npm publish, etc.)
- Tag release (`git tag vX.Y.Z`)
- Push tags to all remotes
- `blocked_by: [LFORGE2-13]`
- `unlocks: [LFORGE2-15]`

**LFORGE2-15: Workspace publish with dependencies**
- `hyperforge workspace publish --path . --bump patch --deps`
- Build dependency graph from LFORGE2-13
- Topological sort (respect local dependencies)
- Publish in correct order:
  1. Packages with no local deps first
  2. Packages depending on (1) next
  3. Continue until all published
- Update dependent packages with new versions
- Commit all version changes
- Push all repos
- `blocked_by: [LFORGE2-13, LFORGE2-14]`
- `unlocks: []`

### Phase 6: Forge Operations (Parallel, requires auth)

**LFORGE2-16: Forge API - Create repository**
- Create repository on forge (GitHub/Codeberg/GitLab)
- Use forge APIs (REST or GraphQL)
- Set visibility, description, homepage
- Return repository URL
- `blocked_by: [LFORGE2-2, LFORGE2-17]`
- `unlocks: []`

**LFORGE2-17: Auth integration**
- Request forge tokens from auth-service
- Handle approval/denial/timeout
- Retry logic
- Store token for session (don't re-request every time)
- `blocked_by: [LFORGE2-2]`
- `unlocks: [LFORGE2-16, LFORGE2-18]`

**LFORGE2-18: Forge API - Update repository**
- Update repository settings on forge
- Change visibility, description, homepage
- Rename repository (if supported)
- `blocked_by: [LFORGE2-17]`
- `unlocks: []`

### Phase 7: Advanced Features (After core complete)

**LFORGE2-19: Add/Remove forge commands**
- `hyperforge add --path . --forge gitlab`
- `hyperforge remove --path . --forge codeberg`
- Update `.hyperforge/config.toml`
- Run sync to reconcile git remotes
- `blocked_by: [LFORGE2-5]`
- `unlocks: []`

**LFORGE2-20: Template system for output**
- Human-friendly output when TTY
- Machine output (JSON) when piped
- `--format json|human|yaml` flag
- Templates or inline formatting
- `blocked_by: [LFORGE2-6]`
- `unlocks: []`

---

## Dependency DAG

```
                         LFORGE2-2 (foundation)
                              │
                    ┌─────────┼──────────┬────────────┐
                    │         │          │            │
                    ▼         ▼          ▼            ▼
                LFORGE2-3  (git)      LFORGE2-17  LFORGE2-13
                    │                  (auth)    (pkg detect)
        ┌───────────┼──────────┐         │            │
        │           │          │         │            ├─────────┐
        ▼           ▼          ▼         ▼            ▼         ▼
    LFORGE2-4   LFORGE2-5  LFORGE2-8  LFORGE2-16 LFORGE2-14  (continue)
     (init)      (sync)     (pull)   (forge API) (publish)
        │           │                              │
        ├───────────┼──────────┐                   │
        │           │          │                   │
        ▼           ▼          ▼                   ▼
    LFORGE2-6   LFORGE2-7  (remotes)          LFORGE2-15
     (status)    (push)                      (workspace pub)
                    │
                    │
                    ▼
                LFORGE2-9
             (workspace disc)
                    │
        ┌───────────┼──────────┐
        │           │          │
        ▼           ▼          ▼
    LFORGE2-10  LFORGE2-11  LFORGE2-12
   (workspace  (workspace  (workspace
     push)       status)      list)
```

**Critical Path**:
LFORGE2-2 → LFORGE2-3 → LFORGE2-4 → LFORGE2-7 → LFORGE2-9 → LFORGE2-10

This is the minimum path to achieve "hyper push" functionality.

**Maximum Parallelism**:
After LFORGE2-3, multiple branches can proceed in parallel:
- Single-repo ops: LFORGE2-4, 5, 6, 7, 8
- Package: LFORGE2-13 → 14 → 15
- Auth: LFORGE2-17 → 16, 18

---

## Implementation Strategy

### Starting Fresh

1. **Create worktree**:
   ```bash
   cd ~/dev/controlflow/hypermemetic
   git worktree add ../hyperforge-v2 -b feat/lforge2-redesign
   cd ../hyperforge-v2/hyperforge
   ```

2. **Delete all source**:
   ```bash
   rm -rf src/*
   # Keep Cargo.toml, modify dependencies as needed
   ```

3. **Start with LFORGE2-2**:
   - Define core types
   - Implement config loading
   - Write tests first

4. **Incremental implementation**:
   - Implement one ticket at a time
   - Test thoroughly before moving on
   - Commit after each ticket
   - Keep tickets small (1-2 days max)

### Testing Strategy

**Unit tests**: Every module
- Config parsing
- Git command parsing
- State reconciliation logic

**Integration tests**: End-to-end scenarios
- Init → push workflow
- Workspace discovery
- Publish with dependencies

**Manual testing**: Real forges
- GitHub/Codeberg API integration
- SSH key handling
- Auth flow

---

## Migration from Old Hyperforge

The old hyperforge uses org-centric YAML configs in `~/.config/hyperforge/`. The new design is completely incompatible.

**Migration strategy: Fresh start**
- Old hyperforge remains as-is
- New hyperforge is a rewrite
- Users adopt repos one at a time with `hyperforge init`
- No automatic migration (too complex, error-prone)

**For ~/.hypermemetic-infra specifically**:
```bash
# For each repo:
cd ~/.hypermemetic-infra/hyperforge
hyperforge init --path . --forges github,codeberg

cd ~/.hypermemetic-infra/substrate
hyperforge init --path . --forges github,codeberg

# ... etc

# Then workspace operations work:
cd ~/.hypermemetic-infra
hyperforge workspace push --path .
```

---

## Success Metrics

After full implementation, these workflows should be smooth:

### 1. New project to multi-forge in 3 commands
```bash
hyperforge init --path . --forges github,codeberg
git add . && git commit -m "Initial commit"
hyperforge push --path .
```

### 2. Publish all packages in workspace with correct order
```bash
hyperforge workspace publish --path . --bump patch --deps
```

### 3. "Hyper push" all repos in a workspace
```bash
hyperforge workspace push --path .
```

### 4. Adopt existing repo in 2 commands
```bash
hyperforge init --path . --forges codeberg
hyperforge push --path .
```

### 5. Check status of all repos
```bash
hyperforge workspace status --path .
# Shows table with repo names, forges, ahead/behind status
```

---

## Open Questions

### 1. Remote naming convention
Should we use:
- **Option A**: `origin`, `github`, `codeberg` (current pattern)
- **Option B**: `origin`, `mirror-github`, `mirror-codeberg`
- **Option C**: Configurable in `.hyperforge/config.toml`

**Recommendation**: Option A (simple, matches current ssh config pattern)

### 2. SSH config integration
Should hyperforge:
- **Option A**: Read from existing SSH config (`~/.ssh/config`)
- **Option B**: Generate SSH config entries
- **Option C**: Just use key names, let user configure SSH

**Recommendation**: Option C (separation of concerns, user controls SSH)

### 3. Workspace root marker
Should we support an optional `.hyperforge-workspace` file to:
- Mark workspace root explicitly
- Store workspace-level settings (default SSH key?)
- Speed up discovery (stop searching at marker)

**Recommendation**: No marker for MVP, consider later if needed

### 4. Forge creation during init
When you run `hyperforge init --path . --forges github`, should it:
- **Option A**: Just configure remotes, assume repo exists on forges
- **Option B**: Create repo on forges if it doesn't exist
- **Option C**: Ask user what to do

**Recommendation**: Option A for MVP, add creation in LFORGE2-16

### 5. Config file location
Should `.hyperforge/config.toml` be:
- **Option A**: `.hyperforge/config.toml` (directory)
- **Option B**: `.hyperforge.toml` (flat file)

**Recommendation**: Option A (allows future expansion: `.hyperforge/cache/`, `.hyperforge/logs/`)

---

## Next Steps

1. **Review this epic** with team/stakeholders
2. **Create worktree** and delete source
3. **Start with LFORGE2-2** (foundation types)
4. **Implement incrementally** following dependency DAG
5. **Test thoroughly** after each ticket
6. **Keep tickets small** (1-2 days max each)

---

## Tickets in this Epic

**Phase 1: Foundation**
- LFORGE2-2: Core types and configuration
- LFORGE2-3: Git integration layer

**Phase 2: Single-Repo Operations**
- LFORGE2-4: Init command
- LFORGE2-5: Sync command
- LFORGE2-6: Status command
- LFORGE2-7: Push command
- LFORGE2-8: Pull command

**Phase 3: Workspace Discovery**
- LFORGE2-9: Workspace discovery

**Phase 4: Workspace Operations**
- LFORGE2-10: Workspace push
- LFORGE2-11: Workspace status
- LFORGE2-12: Workspace list

**Phase 5: Package Publishing**
- LFORGE2-13: Package detection
- LFORGE2-14: Single-repo publish
- LFORGE2-15: Workspace publish with dependencies

**Phase 6: Forge Operations**
- LFORGE2-16: Forge API - Create repository
- LFORGE2-17: Auth integration
- LFORGE2-18: Forge API - Update repository

**Phase 7: Advanced Features**
- LFORGE2-19: Add/Remove forge commands
- LFORGE2-20: Template system for output

**Total: 19 tickets**

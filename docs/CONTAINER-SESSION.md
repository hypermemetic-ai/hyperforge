# Container Session Setup for hyperforge-lforge2

## Overview

This document describes how to run Claude Code in a Docker container for the hyperforge-lforge2 project, including its dependencies (plexus-core, plexus-macros, plexus-transport).

## Problem: Git Worktrees Don't Clone

`hyperforge-lforge2` is a **git worktree** of the main `hyperforge` repo. When claude-container tries to clone it inside Docker, it fails because:

- The `.git` file contains a host path: `gitdir: /Users/user/.../hyperforge/.git/worktrees/hyperforge-lforge2`
- This path doesn't exist inside the Docker container
- Result: Clone hangs/fails with "fatal: not a git repository"

## Solution

Use the **parent repo** (`hyperforge`) in the config instead of the worktree, and specify the branch to checkout.

### Configuration (.claude-projects.yml)

```yaml
version: "1"
projects:
  hyperforge:
    path: ../hyperforge       # Parent repo, not the worktree
    branch: feat/lforge2-redesign  # Branch to checkout
    main: true                # Start Claude in this directory
  plexus-core:
    path: ../plexus-core
  plexus-macros:
    path: ../plexus-macros
  plexus-transport:
    path: ../plexus-transport
```

Key fields:
- `path`: Use parent repo path for worktrees
- `branch`: Specify which branch to clone (claude-container clones with `--branch`)
- `main`: Mark the primary project - Claude will start in `/workspace/hyperforge` instead of `/workspace`

## Usage

### Quick Start

```bash
# From the hyperforge-lforge2 directory
./container-session.sh
```

This script:
1. Creates the git session with all projects
2. Imports the current session transcript (with path rewriting)
3. Sets up sessions-index.json for resume functionality

### Manual Steps

```bash
# Delete existing session (if needed)
~/dev/controlflow/juggernautlabs/claude-container/claude-container --delete-session hyperforge-lforge2 --yes

# Create session
~/dev/controlflow/juggernautlabs/claude-container/claude-container --git-session hyperforge-lforge2 --no-run

# Start container
~/dev/controlflow/juggernautlabs/claude-container/claude-container --git-session hyperforge-lforge2
```

## Workspace Layout (inside container)

```
/workspace/
├── .main-project          # Contains "hyperforge"
├── .claude-projects.yml   # Config copied from host
├── hyperforge/            # Main project (on feat/lforge2-redesign branch)
├── plexus-core/              # Dependency
├── plexus-macros/             # Dependency
└── plexus-transport/         # Dependency
```

Claude starts in `/workspace/hyperforge` because of `main: true`.

## Session Import

The `container-session.sh` script imports the current host session into the container:

1. **Copies transcript** from `~/.claude/projects/-Users-user-dev-controlflow-hypermemetic-hyperforge-lforge2/`
2. **Rewrites paths** from host paths to container paths:
   - `/Users/user/dev/controlflow/hypermemetic/hyperforge-lforge2` → `/workspace/hyperforge`
3. **Creates sessions-index.json** with correct container paths

### Known Issue: Resume Not Working

The `/resume` command may not find imported sessions. This appears to be because Claude Code validates session metadata beyond what's in sessions-index.json. Workaround: use `claude --resume <session-id>` directly.

## claude-container Changes

The following changes were made to claude-container to support this workflow:

### 1. Branch Field Bug Fix (lib/config.sh)

The `validate_config` function was only reading 2 fields from the pipe-delimited format, but the format has 3 fields (`name|path|branch`). Fixed by adding `_branch` to the read:

```bash
while IFS='|' read -r proj_name proj_path _branch; do
```

### 2. Main Project Support (lib/config.sh)

Added `get_main_project()` function that:
- Looks for a project with `main: true` in the config
- Falls back to the first project if none marked

### 3. Store Main Project (lib/git-session.sh)

During session creation, stores the main project name in `/workspace/.main-project`.

### 4. Start in Main Project (claude-container)

Container entrypoint reads `.main-project` and `cd`s to that directory before starting Claude:

```bash
WORK_DIR="/workspace"
if [[ -f /workspace/.main-project ]]; then
    MAIN_PROJ=$(cat /workspace/.main-project)
    if [[ -d "/workspace/$MAIN_PROJ" ]]; then
        WORK_DIR="/workspace/$MAIN_PROJ"
    fi
fi
cd $WORK_DIR
```

## Merging Changes Back

After working in the container, use:

```bash
# Merge container changes to host
~/dev/controlflow/juggernautlabs/claude-container/claude-container --merge-session hyperforge-lforge2

# Or merge to a specific branch
~/dev/controlflow/juggernautlabs/claude-container/claude-container --merge-session hyperforge-lforge2 --into feat/lforge2-redesign
```

## Troubleshooting

### "path does not exist" error with branch

If you see an error like:
```
✗ Project 'hyperforge': path does not exist: /path/to/hyperforge|feat/lforge2-redesign
```

This means the branch parsing bug wasn't fixed. Update claude-container to the latest version.

### Session won't resume

1. Ensure you're in the correct directory (`/workspace/hyperforge`)
2. Check sessions-index.json has the correct `projectPath`
3. Try direct resume: `claude --resume <session-id>`

### Container starts in /workspace instead of /workspace/hyperforge

1. Check `.main-project` file exists: `cat /workspace/.main-project`
2. Verify the main project directory exists: `ls /workspace/hyperforge`
3. Recreate the session with updated config

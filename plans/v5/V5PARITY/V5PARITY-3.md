---
id: V5PARITY-3
title: "GIT — ops::git + transport methods on repos + workspace-parallel variants"
status: Complete
type: implementation
blocked_by: [V5PARITY-2]
unlocks: [V5PARITY-5, V5PARITY-12]
---

## Problem

v5 is metadata-only. No `git clone`, `git fetch`, `git pull`, or git-level `push` of refs. No drift detection against working-tree state. Making v5 a daily driver requires the transport layer.

## Required behavior

**New module: `src/v5/ops/git.rs`.** Wraps `Command::new("git")` (R1 pinned). Inherits the user's SSH agent, credential helper, and hooks — no libgit2. Functions:

| Function | Role |
|---|---|
| `clone_repo(url, dest, transport: Transport)` | shells `git clone <url> <dest>`; `Transport::Ssh` flips URL to `git@host:org/name.git`, `Transport::Https` uses `https://host/org/name.git` |
| `fetch(dir, remote?)` | `git -C <dir> fetch [<remote>]`; all remotes if None |
| `pull_ff(dir, remote, branch)` | `git -C <dir> pull --ff-only <remote> <branch>`; refuses with `dirty_tree` error if working tree dirty |
| `push_refs(dir, remote, branch?)` | `git -C <dir> push <remote> [<branch>]` |
| `status(dir)` | `git -C <dir> status --porcelain=v2 --branch` — parses into `{ ahead, behind, dirty: bool, staged: u32, unstaged: u32, untracked: u32 }` |
| `is_dirty(dir)` | `status(dir).dirty` shortcut |
| `set_remote_url(dir, name, url)` | `git -C <dir> remote set-url <name> <url>` |

**RPC methods on ReposService:**

| Method | Behavior |
|---|---|
| `repos.clone --org X --name N --dest P [--transport ssh\|https]` | calls `ops::git::clone_repo` against first remote. Errors if dest exists. |
| `repos.fetch --org X --name N [--remote URL]` | calls `ops::git::fetch` |
| `repos.pull --org X --name N [--remote URL] [--branch B]` | calls `ops::git::pull_ff` |
| `repos.push_refs --org X --name N [--remote URL] [--branch B]` | distinct from existing `repos.push` (metadata-only). |
| `repos.status --org X --name N --path P` | emits the status snapshot as a `repo_status` event |
| `repos.dirty --path P` | thin — emits `{dirty: bool}` event |
| `repos.set_transport --org X --name N --transport ssh\|https [--path P]` | updates `.git/config` remotes (if path given) AND org yaml's remote URLs |

**Workspace-parallel variants:** `workspaces.{clone, fetch, pull, push_refs}` — iterate members, bounded parallelism (R2: default 4, `--concurrency N` param). Per-member events + aggregate report at end. Partial-failure tolerant (D6).

## What must NOT change

- Existing `repos.push` (metadata-only) unchanged. New method is `repos.push_refs`; these coexist.
- Existing `workspaces.sync` unchanged (metadata sync; distinct from workspace-level git ops).
- `ops::git` is the only module that spawns `git` subprocesses. D13 extended: the V5LIFECYCLE-11 grep adds `Command::new("git")` outside `ops::git` as a violation.

## Acceptance criteria

1. `repos.clone` into a fresh dir produces a working git checkout; `git log` works.
2. `repos.fetch` on a cloned dir succeeds; emits `fetch_done` event.
3. `repos.pull` with local ahead-commits and fast-forward-only refuses with `non_ff` error; with a clean fast-forward proceeds.
4. `repos.status` on dirty tree emits `repo_status { dirty: true, staged: N, ... }`.
5. `repos.set_transport --transport ssh` on an HTTPS-cloned repo flips `.git/config` remote URLs and the org yaml to SSH form; subsequent `repos.fetch` succeeds (auth via SSH agent).
6. `workspaces.clone` on a 3-member workspace clones all 3 under the workspace path; bounded concurrency observed (no more than N parallel processes via `ps`).

## Completion

- Run `bash tests/v5/V5PARITY/V5PARITY-3.sh` → exit 0 (tier 2 — needs tier-2 config + at least one reachable repo).
- Ready → Complete in-commit.

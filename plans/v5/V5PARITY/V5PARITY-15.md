---
id: V5PARITY-15
title: "GIT-ABSTRACTION — git2-backed local ops behind ops::git's typed API"
status: Complete
type: implementation
blocked_by: [V5PARITY-3]
unlocks: []
---

## Problem

V5PARITY-3 chose subprocess `git` over libgit2 to inherit the user's SSH agent, credential helpers, hooks, and gitconfig. That choice is correct for **network and hook-bearing** ops (clone/fetch/pull/push) but pays full subprocess cost for **purely local** reads and writes (status, current branch, origin URL, lightweight tag, commit, add, config-file edits) that need none of those things.

Workspace iterations make this visible: `workspaces.status` over 50 members spawns 50 `git` processes for a read that git2 does in-process from a single open repo handle.

## Required behavior

**`ops::git` becomes a backend-choosing abstraction.** Public API (function names + signatures) stays the contract; the implementation routes per call between subprocess and a new git2-backed local backend.

| Op | Backend | Why |
|---|---|---|
| `clone_repo`, `clone_repo_with_env` | subprocess | needs SSH agent / credential helper / `GIT_SSH_COMMAND` |
| `fetch`, `pull_ff`, `push_refs`, `push_ref` | subprocess | needs auth + hooks |
| `status`, `is_dirty` | git2 | pure local read; called on every workspace iteration |
| `current_branch`, `read_origin_url` | git2 | pure local read (the hand-rolled `.git/config` INI parser in `workspaces.rs` for origin URL goes away) |
| `add`, `commit`, `tag` | git2 | local index/refs ops; no network |
| `show` (rev:path) | git2 | local object read |
| `set_remote_url`, `set_ssh_command`, `clear_ssh_command`, `get_ssh_command` | git2 | local config edits |

**Public API contract (unchanged):** every existing `ops::git::*` function keeps its signature, errors via the same `GitError` variants. Callers (services, build cluster) require no change.

**`Backend` enum or trait, implementer's choice:** how the backend selection is structured (compile-time feature, runtime dispatch, two private modules behind one façade) is up to the implementer. The contract is: callers cannot tell which backend ran.

**Override hatch:** opt-out env var `HF_GIT_FORCE_SUBPROCESS=1` routes every op through subprocess for debugging / regression isolation. Useful when a git2-backed change is suspected of behaving differently from `git`.

## What must NOT change

- D13 / V5LIFECYCLE-11's `command-git` DRY grep — `Command::new("git")` still appears only in `ops/git/*` (the grep may need its path scope tightened from `ops/git\.rs` to `ops/git/.*` if the file is split).
- Public function signatures and `GitError` variants.
- Error semantics — git2's error codes map to the existing `GitError::{NotAGitRepo, DirtyTree, DestExists, NonFastForward, CommandFailed, GitNotFound, Io}` variants. (`CommandFailed` becomes a subprocess-backend-only variant; git2 maps to a new `GitError::Local { code, message }` or reuses `Io`.)
- v5 test outcomes — every existing V5PARITY-{3,9,10,12,13} test stays green with the new backend in place.

## Acceptance criteria

1. `cargo add git2` (or pin a chosen version in `Cargo.toml`); the dep doesn't pull in OpenSSL on the build host (use vendored or system libgit2 — implementer's call, but document the choice in the Cargo.toml comment).
2. `ops::git::status`, `is_dirty`, `current_branch`, `read_origin_url` produce byte-identical event payloads to the V5PARITY-3 subprocess versions across the existing fixtures.
3. `workspaces.status --name W` over a 10-member workspace runs in materially less wall time than the subprocess equivalent (target: <50ms total vs ~10× that for 10 spawns; benchmark in the test).
4. `HF_GIT_FORCE_SUBPROCESS=1` makes every test pass identically — proves the abstraction is bidirectional, not lossy.
5. `read_git_origin` in `src/v5/workspaces.rs` (the hand-rolled INI parser) is deleted; `workspaces.discover` calls `ops::git::read_origin_url` instead.
6. V5LIFECYCLE-11's DRY checkpoint stays green (greps may need scope tightening — see "What must NOT change").

## Completion

- Run `bash tests/v5/V5PARITY/V5PARITY-15.sh` → exit 0 (tier 1).
- Ready → Complete in-commit.

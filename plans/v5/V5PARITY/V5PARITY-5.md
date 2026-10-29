---
id: V5PARITY-5
title: "SSH — per-repo core.sshCommand wiring + repos.set_ssh_key + org fallback"
status: Complete
type: implementation
blocked_by: [V5PARITY-3]
unlocks: [V5PARITY-12]
---

## Problem

v4 writes `core.sshCommand = ssh -i <key>` into each repo's `.git/config`, letting each repo use its own SSH identity without polluting `~/.ssh/config`. v5 relies on the user's ambient SSH setup, which doesn't scale when a single user manages multiple orgs with different keys.

## Required behavior

**New module: `src/v5/ops/git/ssh.rs`** (submodule of V5PARITY-3's `ops::git`):

| Function | Role |
|---|---|
| `set_ssh_command(dir, key_path)` | writes `[core]\n    sshCommand = ssh -i <key> -o IdentitiesOnly=yes` into `<dir>/.git/config` via `git -C <dir> config core.sshCommand <value>`. Idempotent. |
| `clear_ssh_command(dir)` | `git -C <dir> config --unset core.sshCommand` |
| `resolve_key_for(repo, org) -> Option<FsPath>` | selects the SSH key path: per-repo override in `.hyperforge/config.toml` (when V5LIFECYCLE-9's config is in use) > org-level `CredentialEntry` with `type: ssh_key` > None |

**RPC method on ReposService:**

| Method | Behavior |
|---|---|
| `repos.set_ssh_key --org X --name N --path P --key K` | resolves key path (expand ~, validate file exists), writes `core.sshCommand` into the checkout at P. Updates the org yaml's `CredentialEntry` if the user uses `--persist_to_org`. |

**Org-level fallback:** `repos.clone` (V5PARITY-3) consults `ops::git::ssh::resolve_key_for` BEFORE cloning; if a key resolves, sets `GIT_SSH_COMMAND` env for the clone subprocess. After clone completes, writes `core.sshCommand` into the cloned `.git/config` so subsequent `fetch/pull/push` use it.

## What must NOT change

- `~/.ssh/config` is never touched. Hyperforge writes only repo-local config.
- User's existing `GIT_SSH_COMMAND` / `ssh-agent` setup keeps working — v5's writes are strictly additive to `.git/config`, not preemptive.
- V5PARITY-3's clone path still succeeds without an SSH key (falls back to whatever user has configured).

## Acceptance criteria

1. `repos.set_ssh_key --path P --key ~/.ssh/id_foo` writes `core.sshCommand` into `P/.git/config`; re-reading via `git -C P config --get core.sshCommand` returns the expected string.
2. Clone of a repo whose org has an `ssh_key` credential uses that key during the clone (verifiable by `GIT_SSH_COMMAND` trace if SSH_DEBUG is enabled, or by succeeding against a repo only that key can reach).
3. Key path with `~` expansion resolves to the user's home.
4. A nonexistent key file raises a typed `invalid_key` error before any git call.

## Completion

- Run `bash tests/v5/V5PARITY/V5PARITY-5.sh` → exit 0.
- Ready → Complete in-commit.

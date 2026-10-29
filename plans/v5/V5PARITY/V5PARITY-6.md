---
id: V5PARITY-6
title: "LIFECYCLE-EXT — rename, set_default_branch, set_archived + workspace variants"
status: Complete
type: implementation
blocked_by: [V5PARITY-2]
unlocks: [V5PARITY-12]
---

## Problem

Three ForgePort lifecycle operations v4 surfaces as first-class methods are currently achievable in v5 only via the less-ergonomic `repos.push --fields ...` route. Also: six workspace-level methods (check_default_branch, set_default_branch, verify, check, diff, move_repos) have no v5 equivalent.

## Required behavior

**ForgePort trait extension:**

| Method | Input | Output |
|---|---|---|
| `rename_repo(old_ref, new_name, auth)` | `RepoRef`, `RepoName`, `ForgeAuth` | `Result<(), ForgePortError>` |

`set_default_branch` and `set_archived` on the trait are already reachable via `write_metadata` with the corresponding `DriftFieldKind`; this ticket does NOT add distinct trait methods for those.

**RPC methods on ReposService (thin wrappers):**

| Method | Behavior |
|---|---|
| `repos.rename --org X --name N --new_name M` | calls `ops::repo::rename_on_forge` (new in `ops::repo`) which calls `adapter.rename_repo`; on success, updates org yaml entry's name + all members in every workspace yaml that references `X/N`. Atomic across files (tempfile+rename each; if any fails, prior succeeded writes are rolled back). |
| `repos.set_default_branch --org X --name N --branch B` | `ops::repo::write_metadata_on_forge` with `{default_branch: B}` + mirrors to local `RepoMetadataLocal.default_branch` |
| `repos.set_archived --org X --name N --archived true\|false` | same pattern, `archived` field |

**WorkspacesService methods:**

| Method | Behavior |
|---|---|
| `workspaces.set_default_branch --name W --branch B` | for each member: `repos.set_default_branch`. Aggregate report. |
| `workspaces.check_default_branch --name W` | read-only: emits per-member `{ref, declared: B, forge: B'}` + aggregate |
| `workspaces.verify --name W` | emits per-member mismatch events (drift, missing local checkout, SSH key unresolvable, etc.) + aggregate green/yellow/red |
| `workspaces.check --name W` | alias for `verify` with a narrower scope — does `workspaces.sync --dry_run` logic but additionally emits any `config_drift` and protection flag checks |
| `workspaces.diff --name W` | alias for `workspaces.sync --dry_run true` with cleaner output |
| `workspaces.move_repos --name W --from_path P --to_path Q` | moves each checkout directory via `mv` + updates workspace yaml `path` + updates each `.hyperforge/config.toml` in the moved dirs (if present) |

## What must NOT change

- `repos.push` still works for the fields `set_default_branch` and `set_archived` now expose — three ways to accomplish the same thing is fine; the one-method-per-field form is ergonomic sugar.
- Workspace yaml's `repos` list schema unchanged.
- Rename doesn't touch `.hyperforge/config.toml` (it's per-checkout; user re-runs `repos.init` to refresh).

## Acceptance criteria

1. `repos.rename --org X --name N --new_name M` against a real repo renames on GitHub (visible via `gh repo view X/M`), rewrites the org yaml entry, and rewrites every workspace yaml that referenced `X/N`.
2. `repos.set_default_branch --branch develop` changes the branch on the forge + the local `metadata.default_branch`.
3. `repos.set_archived --archived true` archives the forge repo.
4. `workspaces.set_default_branch` iterates all members; aggregate reports success/failure per.
5. `workspaces.diff` on a workspace with one drifted member emits a `sync_diff { status: drifted }` for it AND a `dry_run: true` tag; no forge or yaml writes.
6. `workspaces.move_repos` moves 3 checkouts + updates the workspace `path`; their `.git/config` remote URLs unchanged; their contents identical (byte-for-byte).

## Completion

- Run `bash tests/v5/V5PARITY/V5PARITY-6.sh` → exit 0 (mix: tier 1 for yaml-only paths, tier 2 for forge calls).
- Ready → Complete in-commit.

---
id: V5PARITY-2
title: "IMPORT — list_repos + repos.import + workspaces.discover"
status: Complete
type: implementation
blocked_by: []
unlocks: [V5PARITY-3, V5PARITY-4, V5PARITY-6, V5PARITY-7, V5PARITY-8, V5PARITY-9, V5PARITY-12]
---

## Problem

v5 has no way to ask a forge "what repos exist under this org" or to bootstrap a workspace yaml from an existing directory of git checkouts. Every repo must currently be registered manually via `repos.add`.

## Required behavior

**ForgePort trait extension:**

| Method | Input | Output |
|---|---|---|
| `list_repos(org, auth)` | `OrgName`, `ForgeAuth` | `Result<Vec<RemoteRepo>, ForgePortError>` — stream not required for v1 |

`RemoteRepo` wire shape (new `CONTRACTS §types`): `{ name: RepoName, url: RemoteUrl, default_branch?, description?, archived?, visibility? }` — the forge's view of what we'd register.

**Three adapters implement it.** GitHub `/orgs/{org}/repos` (+ user endpoint fallback per V5PROV-3 pattern), Codeberg `/orgs/{org}/repos`, GitLab `/groups/{org}/projects`. Pagination handled internally; output is the concatenated list.

**New service methods:**

| Method | Purpose |
|---|---|
| `repos.import --org X [--forge F] [--dry_run]` | Walks adapter.list_repos for each configured forge, registers any repos not already in the org yaml. Emits `repo_imported` per new entry + `import_summary` at end. Existing entries are untouched. |
| `workspaces.discover --path P [--org O] [--dry_run]` | FS scan of `P`; for each git dir, read `origin` URL; match against known orgs' repo remote URLs; offer (via `discover_match` events) to bind the local dir to a workspace entry. Creates workspace yaml at `workspaces/<path_basename>.yaml` (name derived) unless `--name` given. |

New events: `repo_imported { ref, url }`, `import_summary { org, total, added, skipped }`, `discover_match { dir, ref, status: matched | orphan | already_member }`, `workspace_discovered { name, path, repo_count }`.

## What must NOT change

- Existing `repos.add` semantics unchanged.
- Existing `workspaces.create` unchanged (discover is a sibling, not a replacement).
- D13 — all network calls go through `ops::repo::*`; the new list_repos wrapper lives in `ops::repo::list_on_forge`.

## Acceptance criteria

1. `repos.import --org X --forge github` against a real org lists every remote repo (paginated); registers all absent ones; `repos.list --org X` afterwards contains the union.
2. Running `repos.import` again is idempotent — zero `repo_imported` events, `import_summary { added: 0, skipped: N }`.
3. `workspaces.discover --path /tmp/ws` on a dir containing clones of registered repos emits `discover_match { status: matched }` per resolved dir + `workspace_discovered`. A dir whose origin doesn't match any registered repo emits `status: orphan`.
4. `dry_run: true` emits the same event stream and writes no yaml.

## Completion

- Run `bash tests/v5/V5PARITY/V5PARITY-2.sh` → exit 0 (tier 1 + tier 2 combined — tier 1 does discover against a seeded fixture; tier 2 does import against the real sandbox).
- Ready → Complete in-commit.

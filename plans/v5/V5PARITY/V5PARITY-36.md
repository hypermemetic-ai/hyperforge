---
id: V5PARITY-36
title: "REPOS-MIGRATE — typed retire/migrate RPC across forges"
status: Complete
type: implementation
blocked_by: [V5PARITY-34, V5PARITY-35]
unlocks: []
---

## Problem

The retirement workflow ("move a repo from github to codeberg/hyperslop, scope the original to `forges:[]`") was just executed via:

1. `curl POST /api/v1/repos/migrate` with two tokens stitched in shell
2. `synapse … repos add` to register the new dest
3. `synapse … repos set_forges --forges none` to retire the source

That shell loop tripped over a clipboard token swap, falsely matching `"does not exist"` against `"already exists"` and silently reporting 15 successes when 0 had actually migrated. Real bug, real data risk. The flow is common enough — moving repos between forges, retiring to an archive org — that it should be a typed RPC with proper error handling, secret resolution, and event surface.

## Required behavior

**`ForgePort::migrate_from(source_url, dest_repo_ref, options, source_auth, auth)` trait method.** Codeberg/Gitea implements via `POST /api/v1/repos/migrate`. GitHub returns `Unimplemented` (no import-from-git API). GitLab implementation can land later.

**`MigrateOptions`** — closed struct: `{ private: bool, description: String, mirror: bool }`. `mirror: true` makes codeberg pull-sync from the source; `false` is a one-shot copy (the retirement default).

**`repos.migrate --org X --name N --to <provider>/<dest-org>[/<rename>] [...]`**

| Param | Meaning |
|---|---|
| `--to github/foo` etc. | destination provider + org. Optional `/rename` suffix renames at destination (default: same name). |
| `--mirror bool` | one-shot copy (`false`, default) vs continuous pull mirror (`true`) |
| `--private bool` | destination visibility (`true` for retirement, default; matches "archive" intent) |
| `--retire bool` | after migrate succeeds, scope source's `forges: []` so v5 stops syncing to source. Default `true`. |
| `--archive_source bool` | also call `repos.set_archived --archived true` on the source BEFORE retirement. Default `false`. |
| `--dry_run bool` | preview events without writing to either forge or v5 |

**Per-stage events (named, closed enum):**
- `migrate_started { ref, source_url, dest_provider, dest_org, dest_name }`
- `forge_migrated { ref, dest_url, size_bytes? }` — codeberg returns the new repo
- `repo_added { dest_ref }` — same shape as existing `repos.add`'s repo_added
- `forges_set { ref, forges: [], changed }` — same shape as `set_forges`'s forges_set when `--retire true`
- `archived_set { ref, archived: true }` — when `--archive_source true`
- `migrate_done { source_ref, dest_ref, retired: bool, archived: bool }`

**Failure stages map to a closed `MigrateStage` enum:** `precheck | source_resolve | forge_migrate | v5_register | retire | archive`. On failure: emit partial events plus `migrate_failed { stage, message }`. Caller can inspect what landed.

**`workspaces.migrate --name W --to <provider>/<org> [--filter G] [...]`** — same shape, iterates filtered members. Aggregate `workspace_migrate_summary { total, ok, errored, skipped }`.

## What must NOT change

- D9 secret redaction — tokens never appear in event payloads or stderr.
- D13 — only `ForgePort` adapters do forge HTTP; `repos.migrate` orchestrates.
- V5PARITY-34's `forges` filter semantics — applied via the existing `set_forges` path.

## Acceptance criteria

1. `repos.migrate --org hypermemetic --name foo --to codeberg/hyperslop` against a real github source emits the per-stage events and ends with `migrate_done`. Codeberg now has `hyperslop/foo`. v5 `hyperslop.yaml` lists it. v5 `hypermemetic.yaml`'s entry has `forges: []`.
2. `--retire false` skips the source scoping; source stays unchanged.
3. `--archive_source true` archives the github source before retiring.
4. `--mirror true` creates a continuous pull mirror on codeberg (verifiable via `gh api /repos/hyperslop/foo --jq .mirror`).
5. `--dry_run true` emits all events with no forge HTTP and no v5 yaml writes.
6. Source token comes from v5's secret store via the source org's existing credential resolution (V5PARITY-24's resolve chain). No clipboard / no env var dependency.
7. `repos.migrate --to github/foo` emits `migrate_failed { stage: forge_migrate, message: "github does not support migrate_from" }`.
8. `workspaces.migrate --name W --filter "axon,service-*" --to codeberg/hyperslop --retire true` retires every filter-matching member; aggregate `workspace_migrate_summary { ok, errored, skipped }`.

## Completion

- Run `bash tests/v5/V5PARITY/V5PARITY-36.sh` → exit 0 (tier 1 against a local gitea instance OR a stub HTTP server; tier 2 against real codeberg).
- Ready → Complete in-commit.

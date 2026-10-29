---
id: V5LIFECYCLE-3
title: "ops::repo::sync_one — single pure sync, called by both services"
status: Complete
type: implementation
blocked_by: [V5LIFECYCLE-2]
unlocks: [V5LIFECYCLE-4]
---

## Problem

`repos.sync` (V5REPOS-13) and `workspaces.sync` (V5WS-9 + V5PROV-8) both implement the same inner loop: resolve provider → build `ForgeAuth` → call `adapter.read_metadata` → compute drift → build a SyncDiff. The two copies diverge only in which event envelope wraps the result. Per D13 this logic lives once.

## Required behavior

Introduce a pure function in `src/v5/ops/repo` (exact path free) with this capability:

| Input | Type | Notes |
|---|---|---|
| repo | `&OrgRepo` | the repo entry from org yaml |
| org_cfg | `&OrgConfig` | for credential lookup |
| provider_map | `&BTreeMap<DomainName, ProviderKind>` | from global config |
| resolver | `&dyn SecretResolver` | V5CORE-4 |
| remote_filter | `Option<&RemoteUrl>` | when set, only this one remote |

| Output / Result | Shape |
|---|---|
| `SyncOutcome` | per-remote list of `SyncOutcomeEntry` |
| `SyncOutcomeEntry` | `{ url, provider, result: SyncStatus, drift: Vec<DriftField>, remote: Option<ForgeMetadata>, error_class: Option<ForgeErrorClass> }` |

`SyncStatus` reuses CONTRACTS §types: `in_sync`, `drifted`, `errored`. No `created` — that's V5PROV-8 / ops::repo::ensure_exists territory (V5LIFECYCLE-4).

Migration:
- `ReposService::sync` calls `ops::repo::sync_one`, iterates the `SyncOutcomeEntry` vec, translates each to a `RepoEvent::SyncDiff`.
- `WorkspacesService::sync` calls `ops::repo::sync_one` **per workspace member**, collapses the per-remote entries into one per-member SyncDiff (per the V5WS-9 shape contract — "one SyncDiff event per member"), translates to `WorkspacesEvent::SyncDiff`.
- The `compute_drift` function moves to `ops::repo` (or stays private and is called by `sync_one` only). Callers outside `ops` don't reference it.

## What must NOT change

- RepoEvent::SyncDiff wire shape — same fields, same `type: "sync_diff"` discriminator.
- WorkspacesEvent::SyncDiff wire shape — same.
- Workspaces.sync's one-event-per-member contract (V5WS-9).
- Behavior when a member has multiple remotes: v5 currently picks the first remote as the canonical sync target for the workspace-level event; this stays.

## Acceptance criteria

1. Tier-1 sweep passes green; counts identical to pre-ticket.
2. `grep -RE 'adapter\.read_metadata|compute_drift' src/v5/{repos,workspaces}.rs` returns empty — both files call through `ops::repo::*` only.
3. `tests/v5/V5LIFECYCLE/V5LIFECYCLE-3.sh` runs `hf_cmd repos sync` and `hf_cmd workspaces sync` on the same fixture, asserts the per-remote SyncDiff from the former exactly contains (as a subset) the per-member SyncDiff payload from the latter for the same repo (proof the two paths compute the same drift).

## Completion

- Run `bash tests/v5/V5LIFECYCLE/V5LIFECYCLE-3.sh` → exit 0.
- Status flips in-commit.

---
id: V5LIFECYCLE-1
title: "v5 Lifecycle — soft-delete, purge, protection, .hyperforge config, and the ops layer"
status: Epic
type: epic
blocked_by: []
unlocks: []
---

## Goal

Close two related gaps in v5:

1. **Deletion semantics.** v5's current `repos.delete` (V5PROV-7, since reverted) was a hard-delete that called `adapter.delete_repo` immediately. v4's actual behavior is a **soft-delete**: privatize on remote + mark the local record `dismissed` + keep it in `orgs/<org>.yaml`. A separate `repos.purge` does the hard delete. Protected repos refuse both.
2. **Per-repo identity.** v5 has no `.hyperforge/config.toml`. v4 uses it as a secondary identity source when scanning workspaces. Without it, workspaces can't self-describe and `repos.init` has nothing to emit.

A third, cross-cutting concern rides along: **DRY**. Before V5PROV we grew duplicated logic in `repos.sync` vs `workspaces.sync`, duplicated create-check paths, duplicated yaml I/O. This epic refactors every repo-level operation into a pure `ops::*` library layer (D13) before extending it with new lifecycle behavior — so both `ReposService` and `WorkspacesService` call into the same code.

## Dependency DAG

```
Phase A — refactor (no behavior change)

  V5LIFECYCLE-2  ops::state     — shared yaml I/O + lookups
        │
  V5LIFECYCLE-3  ops::repo::sync_one
        │
  V5LIFECYCLE-4  ops::repo::{exists,create,delete}_on_forge
        │
Phase B — new capability (builds on the clean base)

  V5LIFECYCLE-5  RepoLifecycle + ops::repo::{dismiss,purge}
        │
  V5LIFECYCLE-6  repos.delete → soft (privatize + dismiss)
  V5LIFECYCLE-7  repos.purge   → hard (requires dismissed)
  V5LIFECYCLE-8  repos.protect
        │    (5/6/7/8 can overlap once 5 lands)
        │
  V5LIFECYCLE-9  repos.init + HyperforgeRepoConfig (.hyperforge/config.toml)
        │
  V5LIFECYCLE-10 workspaces.{reconcile,sync} consult .hyperforge/config.toml
        │
  V5LIFECYCLE-11 Checkpoint — lifecycle matrix + DRY grep invariant
```

## Tickets

| ID | Status | Phase | Summary |
|----|--------|-------|---------|
| V5LIFECYCLE-2  | Pending | A | Extract `ops::state` — shared yaml I/O |
| V5LIFECYCLE-3  | Pending | A | Extract `ops::repo::sync_one`; repos.sync & workspaces.sync both call it |
| V5LIFECYCLE-4  | Pending | A | Extract `ops::repo::{exists,create,delete}_on_forge` |
| V5LIFECYCLE-5  | Pending | B | Add `RepoLifecycle` + `privatized_on` + `protected` to repo record |
| V5LIFECYCLE-6  | Pending | B | `repos.delete` soft semantics (D12) |
| V5LIFECYCLE-7  | Pending | B | `repos.purge` — hard-delete, gated on dismissed |
| V5LIFECYCLE-8  | Pending | B | `repos.protect` — guard toggle |
| V5LIFECYCLE-9  | Pending | B | `repos.init` — writes `.hyperforge/config.toml` |
| V5LIFECYCLE-10 | Pending | B | `workspaces.{reconcile,sync}` consult `.hyperforge/config.toml` |
| V5LIFECYCLE-11 | Pending | B | Checkpoint — lifecycle matrix + DRY grep |

## What must NOT change

- **V5 tier-1 regression.** Every currently-passing test must still pass after each ticket. Phase A is explicitly a zero-behavior-change refactor: run the full suite after each of V5LIFECYCLE-2/3/4 and confirm the same set passes.
- **D11 create-side semantics.** `repos.add --create_remote`, visibility, description params unchanged. Only delete-side semantics are replaced (by D12).
- **V5PROV-6/8** behaviors. Auto-create on workspace sync (V5PROV-8) stays. It will migrate to `ops::repo::create_on_forge` (V5LIFECYCLE-4) with no wire-observable change.
- **`adapter.delete_repo`** stays on ForgePort (from V5PROV-2). It's reachable only through `ops::repo::delete_on_forge`, which is called only from `repos.purge`.

## Out of scope (future epics)

- `repos.import` / `repos.status` / `repos.clone` (V5PARITY)
- `workspaces.discover` (bootstrap workspace yaml from filesystem)
- CI / dist / large-file-threshold fields on `.hyperforge/config.toml` — deliberately narrower than v4's HyperforgeConfig for v1
- Multi-forge privatization races (ticket notes this as a known limitation)

## Risks

- **R1: Privatization partial-success.** A repo on github + codeberg + gitlab may privatize on 1 of 3, leaving a mixed state. V5LIFECYCLE-6 must define: partial-success is still a successful delete (record marked dismissed, `privatized_on` carries the successful subset), but an event per failed forge is emitted.
- **R2: Workspace sync semantics on dismissed repos.** V5WS-9 + V5PROV-8 assume every member wants its remote metadata synced. For `dismissed` members this is wasted API call. V5LIFECYCLE-10 must decide: skip dismissed members in sync (default), or include (a new `--include_dismissed` flag)?
- **R3: `.hyperforge/config.toml` provenance conflict.** If the file says one org, the org yaml says another, which wins? D14 pins: org yaml wins; reconcile emits `config_drift` event.

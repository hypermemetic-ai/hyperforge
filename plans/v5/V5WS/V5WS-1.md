---
id: V5WS-1
title: "Hyperforge v5 Workspaces — CRUD, Reconcile, Sync"
status: Epic
type: epic
blocked_by: [V5CORE-3, V5CORE-8, V5CORE-9]
unlocks: []
---

## Goal

Populate the `WorkspacesService` stub (registered in V5CORE-8) with the full
workspace lifecycle plus two higher-level operations: `reconcile` (detect
dir renames and filesystem removals, update workspace yaml accordingly)
and `sync` (orchestrate `repos.sync` across workspace members).

When this epic is done:

- A workspace is a named record with a filesystem path and a list of
  `<org>/<name>` repo references.
- Workspace lifecycle (`list`, `get`, `create`, `delete`, `add_repo`,
  `remove_repo`) operates entirely via synapse — no hand-editing of
  `~/.config/hyperforge/workspaces/<ws>.yaml`.
- `reconcile` walks the workspace path, matches each git dir to a known
  repo via remote URL, and updates the workspace yaml to reflect any dir
  renames or removals — without ever mutating the filesystem or the
  forge.
- `sync` delegates to `repos.sync` for each member and aggregates the
  per-repo results into a workspace-level report.
- Destructive operations (delete workspace, remove repo) never touch the
  forge unless `delete_remote: true` is passed — default is always safe.

## Dependency DAG

```
              V5CORE-3, V5CORE-8, V5CORE-9
                         │
                         │  (epic unblocked)
                         │
  ┌──────────┬───────────┼───────────┬──────────┬──────────┬──────────┬──────────┐
  │          │           │           │          │          │          │          │
V5WS-2    V5WS-3     V5WS-4      V5WS-5     V5WS-6     V5WS-7     V5WS-8
(ws.list) (ws.get)  (ws.create) (ws.       (ws.add_   (ws.remove_ (ws.reconcile)
                                delete)    repo)      repo)
  │          │           │           │          │          │          │
  └──────────┴───────────┴───────────┼──────────┴──────────┴──────────┘
                                     │
                                     │  (and separately: V5REPOS-13 landed)
                                     │
                                  V5WS-9 (ws.sync — orchestrates repos.sync)
                                     │
                                  V5WS-10 (WS checkpoint)
```

**Phase 1 (7-way parallel):** V5WS-2, 3, 4, 5, 6, 7, 8. Every CRUD ticket
owns one method against the workspace YAML. Reconcile (V5WS-8) is also in
this phase — it reads filesystem + org yamls + workspace yaml, and writes
only workspace yaml. No method in phase 1 reads another phase-1 method's
output at runtime.

**Phase 2 (single ticket, cross-epic dep):** V5WS-9 orchestrates `repos.sync`
across workspace members. Unblocks only after V5REPOS-13 lands.

**Phase 3 (checkpoint):** V5WS-10.

## Tickets

| ID | Status | Summary |
|----|--------|---------|
| V5WS-2  | Pending | `workspaces.list` — summary per workspace |
| V5WS-3  | Pending | `workspaces.get <name>` — full detail including resolved repo refs |
| V5WS-4  | Pending | `workspaces.create <name>` — writes new workspace yaml |
| V5WS-5  | Pending | `workspaces.delete <name>` — removes workspace yaml; `delete_remote: bool` for cascading forge deletion |
| V5WS-6  | Pending | `workspaces.add_repo <name> <org>/<repo>` |
| V5WS-7  | Pending | `workspaces.remove_repo <name> <org>/<repo>` — `delete_remote: bool` defaults false |
| V5WS-8  | Pending | `workspaces.reconcile <name>` — rename + rm detection, workspace-yaml-only mutation |
| V5WS-9  | Pending | `workspaces.sync <name>` — orchestrates `repos.sync` over members, aggregates results |
| V5WS-10 | Pending | WS checkpoint: user-story verification + state map |

## User stories (the checkpoint verifies these)

1. **Stand up a workspace.** Given existing orgs and repos, I create a
   new workspace named `main` at `/path/to/dev` containing three repo refs.
2. **Rename a dir locally, reconcile.** I `mv repo-a repo-a-fork` in the
   workspace path. `reconcile` detects the rename via matching remote URLs
   and updates the workspace yaml from `hypermemetic/repo-a` string form
   to `{ref: hypermemetic/repo-a, dir: repo-a-fork}` object form. Neither
   the filesystem nor any forge is touched.
3. **Delete a dir locally, reconcile.** I `rm -rf repo-b`. `reconcile`
   detects the missing dir and drops the entry from the workspace yaml.
   The repo remains in the org yaml; the forge remains untouched.
4. **Remove with cascade.** `workspaces.remove_repo main hypermemetic/c
   --delete_remote true` removes the dir, drops the workspace entry, *and*
   deletes the repo on its forge. Without `--delete_remote true`, only the
   workspace entry changes.
5. **Sync a workspace.** `workspaces.sync main` runs `repos.sync` on every
   member and returns a single aggregated report: per-repo in-sync /
   drifted / errored, plus a workspace-level summary.
6. **Cross-org workspace.** A workspace references repos from two
   different orgs. Every method handles this correctly — the `<org>/<repo>`
   ref is the unit of membership, not the org.

## Contracts pinned here

- **Workspace yaml shape.** Pinned in V5WS-4. Downstream tickets
  (list/get/add_repo/etc.) all read and write this shape.
- **Repo ref string form.** Pinned in V5WS-6. Format: `<org>/<name>`.
  V5WS-8 (reconcile) rewrites entries from string form to object form
  when a dir rename is detected.
- **Reconcile event shape.** Pinned in V5WS-8. Describes what changed
  per repo (renamed | removed | unchanged | new-matched). V5WS-10
  checkpoint asserts these events.
- **Workspace sync report shape.** Pinned in V5WS-9. Combines per-repo
  `repos.sync` results (shape owned by V5REPOS-13) into a workspace
  aggregate.

## What must NOT change

- v4's `workspace.*` activation. v5's workspaces live in a new config
  subdir (`~/.config/hyperforge/workspaces/`) that v4 does not touch.
  Neither daemon should see the other's state.
- Org yaml files. v5 workspaces *read* `orgs/<org>.yaml` to resolve repo
  refs and remote URLs, but never mutate them. Only V5ORGS and V5REPOS
  tickets write to org yamls.

## Risks

- **R1: Reconcile false positives.** Two local dirs might legitimately
  share a remote (e.g., two worktrees of the same repo). V5WS-8 must
  define which dir wins — first match, most-recently-modified, or
  explicit `primary` flag on the workspace entry? Pin before landing.
- **R2: Workspace sync concurrency.** Orchestrating `repos.sync` for N
  repos sequentially is slow for N=44. V5WS-9 must pin a concurrency
  model — serial, fixed-parallelism, or unbounded. Pick one; unbounded
  will hit forge rate limits.
- **R3: Partial failure semantics.** If 10 of 44 member syncs fail, is
  the workspace sync "failed" or "partial"? V5WS-9 must pin — the
  aggregate report shape from contracts above is where this surfaces.

## Out of scope

- Workspace discovery (walk disk, auto-create a workspace from existing
  git clones). Useful but post-v5 — belongs in a `discover` epic that
  may compose REPOS and WS primitives.
- Per-workspace branch policies, transport default, excludes. Deferred
  explicitly — workspaces are extensible, v1 stays minimal.
- Workspace rename. Low-frequency; deleting + recreating is acceptable
  in v1.
- `workspaces.push` (mirror of workspace-level sync for push direction).
  If demand exists after v5 ships, a single follow-up ticket adds it —
  V5REPOS-14 already provides the per-repo primitive.

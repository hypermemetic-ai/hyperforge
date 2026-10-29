# Hyperforge v5

A ground-up rewrite of the hyperforge plugin — multi-forge repo registry,
metadata sync, and workspace orchestration over GitHub, Codeberg, and
GitLab. v5 runs as a separate Plexus daemon on port **44105** under the
registry name **`lforge-v5`**, alongside the v4 daemon on port 44104.
The two share no code; v4 stays usable until you choose to retire it.

The design invariants, decisions, and types are pinned in
[`plans/v5/README.md`](../../plans/v5/README.md) and
[`plans/v5/CONTRACTS.md`](../../plans/v5/CONTRACTS.md). Source lives at
`src/v5/`.

## What's shipped

As of this writing, 59 tickets across the seven epics are Complete:

| Epic | Surface |
|---|---|
| V5CORE | crate scaffold, config loaders, embedded secret store, three child services, test harness |
| V5ORGS | `orgs.{list,get,create,update,delete,set_credential,remove_credential}` |
| V5REPOS | `repos.{list,get,add,remove,add_remote,remove_remote,sync,push}` + `ForgePort` trait + GitHub/Codeberg/GitLab adapters |
| V5WS | `workspaces.{list,get,create,delete,add_repo,remove_repo,reconcile,sync}` |
| V5PROV | forge-side `create_repo`/`delete_repo`/`repo_exists`, `repos.add --create_remote`, `workspaces.sync` auto-create |
| V5LIFECYCLE | `ops::` library layer (D13), soft-delete (`repos.delete`), `repos.{purge,protect,init}`, `.hyperforge/config.toml` |
| V5PARITY | `repos.import` + `workspaces.discover` (V5PARITY-2); `repos.{clone,fetch,pull,push_refs,status,dirty,set_transport}` + workspace-parallel variants (V5PARITY-3) |

What's not yet shipped: analytics (size/loc/large-files), SSH key wiring,
lifecycle-ext (rename, set_default_branch, set_archived,
workspaces.verify/check/diff/move_repos), `secrets.set` + auth_check,
CLI ergonomics, and the build/release pipeline. v4 still owns those.

## Where to read next

- [Getting started](./getting-started.md) — install, add an org, create
  a workspace, sync.
- [Methods reference](./methods.md) — every shipped RPC method by service
  with parameters and the events it emits.
- [Architecture](./architecture.md) — `ops::` layer, `ForgePort`,
  `SecretResolver`, how a service method translates into ops calls.
- [Data model](./data-model.md) — strong types, on-disk YAML shapes,
  `.hyperforge/config.toml`.
- [v4 vs v5](./v4-vs-v5.md) — what changed, why no migration tool exists.
- [Development](./development.md) — running the test harness, DRY
  invariants, the one-pass implementation cadence.

## What v5 deliberately does NOT do (yet)

- `build.*` activation (out of scope for v5 — port forward in a future
  epic).
- `hyperforge-auth` sidecar — collapsed into the daemon; YAML-only
  secret backend in v1.
- SSH key management CLI (v4's `hyperforge-ssh`).
- MCP HTTP server mode.
- Dynamic-children activation tree (deferrable; static-only in v1).
- Selector / fan-out CLI syntax.
- Multi-tenant `user` layer above `org`.

Each is a legitimate future epic. None is a prerequisite for the v5
surface to be useful.

## Status invariant

v5's port (44105) and registry name (`lforge-v5`) are pinned by D1 in
CONTRACTS. v4 keeps port 44104 and registry name `lforge`. They never
collide; there is no shared global state.

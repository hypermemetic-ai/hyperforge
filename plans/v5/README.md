# Hyperforge v5 — Rewrite Roadmap

A ground-up rewrite of the hyperforge plugin with a tightened data model,
static-only activation tree (dynamic children deferred), and end-to-end tests
driven through synapse.

Not an in-place refactor of v4. v5 lives alongside v4 until v5's checkpoint
tickets ship green, at which point v4 is retired in a separate cleanup epic
(not part of this roadmap).

## Design invariants

The following are locked before epic drafting begins. They constrain every
epic and every ticket. If a ticket contradicts one, the ticket is wrong.

1. **Data model.**
   - `org { name, forge { provider, credentials[]{key, type} }, repos[] }`
   - `repo { name, remotes[] }` (per-org; remote URL domain → provider via
     the global provider map, with a per-remote `provider:` override escape
     hatch)
   - `workspace { name, path, repos[] }` where entries are `<org>/<name>` refs
     (string shorthand when local dir name matches repo name, `{ref, dir}`
     object form when it differs)
   - Credentials are strictly per-org. No per-repo cred overrides.
   - Workspaces are extensible — only `name`, `path`, `repos[]` are pinned for v1.

2. **Config layout.**
   ```
   ~/.config/hyperforge/
   ├── config.yaml         # global: default_workspace, provider domain map
   ├── orgs/<org>.yaml     # one file per org
   ├── workspaces/<ws>.yaml
   └── secrets.yaml        # resolved via secrets:// references
   ```

3. **Activation tree (v1).** Static-only — no `#[child(list=...)]`, no
   `list_children`/`search_children`. Upgrade path to dynamic children is
   additive in plexus-macros 0.5, so this decision is deferrable, not
   foreclosed.

   ```
   HyperforgeService
   ├─ status                 (method)
   ├─ orgs                   (static child) → OrgsService
   ├─ repos                  (static child) → ReposService
   └─ workspaces             (static child) → WorkspacesService
   ```

   Root has no CRUD methods. Lifecycle lives on the per-noun services. Instance
   names are explicit parameters on every method that needs one (`org`,
   `name`, `repo_ref`) — never positional CLI segments.

4. **Destructive operations are opt-in.** Plain filesystem `rm` is always
   safe — reconcile silently drops the entry. Forge-side deletion requires
   an explicit boolean parameter (`delete_remote: true`).

5. **No build/release pipeline in v5 scope.** v4's `build.*` activation is
   out of scope for the rewrite. A post-v5 epic may port it. Users depending
   on build commands keep v4 running in parallel until then.

6. **Tests run against a real daemon via synapse.** Unit tests of Rust
   functions are not the acceptance bar. Each ticket's acceptance criteria
   are scripted as bash invocations of `synapse -P <port> lforge hyperforge
   …`, asserting on stdout. Rust may invoke those bash scripts as its test
   harness — the contract is that the assertion is made at the synapse
   surface, not below it. See `V5CORE-11` for the shared harness.

7. **No auth sidecar.** v4's `hyperforge-auth` is collapsed into the main
   daemon. Secret resolution is an internal trait with a YAML backend for
   v1. OS keyring / other backends are post-v5.

## Epics

| Epic | Title | Unlocks |
|------|-------|---------|
| [V5CORE](./V5CORE/V5CORE-1.md)   | Scaffolding, config, harness, service stubs | all others |
| [V5ORGS](./V5ORGS/V5ORGS-1.md)   | OrgsService — CRUD + credentials             | V5WS fixtures |
| [V5REPOS](./V5REPOS/V5REPOS-1.md) | ReposService — CRUD + ForgePort + adapters  | V5WS-sync |
| [V5WS](./V5WS/V5WS-1.md)         | WorkspacesService — CRUD + reconcile + sync | — |

## Cross-epic dependency DAG

```
                             V5CORE-2 (crate scaffold, plexus 0.5 wiring)
                                        │
        ┌───────────────────────────────┼───────────────────────────────┐
        │                               │                               │
   V5CORE-3                        V5CORE-4                        V5CORE-5..9
   (config loaders                 (embedded                       (status, three service
    — yaml schemas,                 secret store,                   stubs, test
    round-trip)                     secrets:// ref                  harness)
        │                            resolver)
        └─────────────┬──────────────────┬────────────────────────────┘
                      │                  │
              (shared prereqs: V5CORE-3 + V5CORE-4 + V5CORE-9 + relevant stub)
                      │
        ┌─────────────┼──────────────┬──────────────────┐
        │             │              │                  │
      V5ORGS       V5REPOS         V5WS              V5CORE-10
      epic         epic            epic              (CORE checkpoint —
        │             │              │                blocks none of
        │             │              │                the downstream
        │             │              │                epics; verifies
        │             │              │                CORE's own surface)
        │             │              │
        │             │   ┌──────────┼──────────┐
        │             │   │          │          │
        │             │  WS-2..8   WS-9        WS-10
        │             │  (parallel) (needs     (checkpoint)
        │             │             REPOS-13)
        │             │              │
        │             └──────────────┤
        │                            │
        │     ┌────────────┐         │
        │     │ REPOS-13   │─────────┘
        │     │ (repos.sync)│
        │     └──────┬─────┘
        │            │
        │   (REPOS-13 needs REPOS-2 + ≥1 adapter)
        │
      ORGS-9 (ORGS checkpoint)
```

### Parallelism summary

Each epic's overview shows its internal DAG. At peak, the graph supports:

- **CORE phase 1**: 1 ticket (V5CORE-2 scaffold)
- **CORE phase 2**: 7 tickets in parallel (V5CORE-3..9)
- **Downstream epics phase 1**: three epics in parallel (ORGS, REPOS, WS), each
  exposing its own parallel front:
  - ORGS: 7 parallel tickets
  - REPOS: 8 parallel tickets (CRUD + trait + URL derivation)
  - WS: 7 parallel tickets
- **REPOS phase 2**: 3 parallel adapter tickets
- **REPOS phase 3**: 2 parallel (sync, push)
- **WS phase 2**: 1 ticket (workspace sync, after REPOS-13)
- **Checkpoint phase**: all three epic checkpoints + CORE checkpoint can run in
  parallel after their respective implementation tickets land

Theoretical peak concurrency: ~22 tickets simultaneously in parallel during the
downstream CRUD wave. Actual concurrency is bounded by implementer count;
the DAG exposes the opportunity, it does not require it.

## Methodology

All tickets start `status: Pending`. Only the user promotes to `Ready`. No
implementation begins until promotion. After tickets are drafted, the
`epic-evaluation` skill runs per-epic as the promotion gate — catches
illustrative-not-pinned types, soft acceptance criteria, and hidden design
decisions before any cycle is burned.

## Out of scope (v5)

- `build.*` activation (validate, bump, publish, release, release_all, etc.)
- `repo init` / `repo set_transport` per-repo config file (`.hyperforge/config.toml`)
- SSH key management CLI (v4's `hyperforge-ssh` binary)
- `hyperforge-auth` sidecar binary
- MCP HTTP server mode
- Dynamic children + search (deferrable; additive in plexus-macros)
- Selector/fan-out syntax (post-v5 synapse feature)
- Multi-tenancy (`user` layer above `org`)

Each of these is a legitimate future epic; none are prerequisites for the v5
surface to be useful.

# v4 vs v5

v5 is a ground-up rewrite, not an in-place refactor. v4 and v5 coexist:
v4 keeps port 44104 + registry name `lforge`; v5 takes 44105 +
`lforge-v5`. They share no runtime state and no config files. Migration
between them is **deliberately not provided** — the user authors v5
configs fresh, and existing v4 configs stay on v4 until the user is
ready to retire that side.

## Process model

| | v4 | v5 |
|---|---|---|
| Daemon binary | `hyperforge` | `hyperforge-v5` |
| Default port | 44104 | 44105 (D1) |
| Plexus registry name | `lforge` | `lforge-v5` (D1) |
| Auth sidecar | `hyperforge-auth` (port 4445, namespace `secrets`) | none — embedded `YamlSecretStore` (D2) |
| SSH CLI | `hyperforge-ssh` | not in v5 (V5PARITY-5 will revisit) |

## Config layout

v4's TOML-heavy, two-file split is replaced with a single YAML shape
per noun.

| File | v4 | v5 |
|---|---|---|
| Global config | n/a | `~/.config/hyperforge/config.yaml` (v5-only — global provider map + default workspace) |
| Org | `orgs/<org>.toml` (TOML) | `orgs/<org>.yaml` (YAML) |
| Repo registry | `orgs/<org>/repos.yaml` (separate `LocalForge` file) | merged inline into the org yaml as `repos[]` |
| Workspaces | implicit (`OrgConfig.workspace_path` + filesystem scan) | `workspaces/<ws>.yaml` (v5-only, first-class) |
| Secrets | `secrets.yaml` (accessed via `hyperforge-auth`) | `secrets.yaml` (file-compatible shape, accessed via embedded `YamlSecretStore`) |
| Per-repo | `<repo>/.hyperforge/config.toml` | `<repo>/.hyperforge/config.toml` — same path, same TOML format, **narrower schema** (`ci`, `large_file_threshold_kb`, `dist`, `ssh`, `forge_config` are dropped) |

A v4-written `.hyperforge/config.toml` carrying any of the dropped
fields fails v5's `#[serde(deny_unknown_fields)]` parse. v5 expects
that file to be authored by `repos.init` against a v5 daemon.

## Activation tree

v4 (`HyperforgeService`):

```
status, reload, begin, orgs_*, config_*, auth_*
├─ repo.*       (CRUD + git transport + inspection)
├─ workspace.*  (8-phase sync, discover, init, check, diff, verify, ...)
└─ build.*      (manifest, release, dist, run, ...)
```

v5 (`HyperforgeService`):

```
status
├─ orgs.*        (CRUD + credentials)
├─ repos.*       (CRUD + ForgePort sync + lifecycle + git transport)
└─ workspaces.*  (CRUD + reconcile + sync + git transport)
```

Notable shape changes:

- **Lifecycle on the per-noun services**, not the root. v4 had `orgs_*`
  and `config_*` on root; v5 puts CRUD on the service the noun belongs to.
- **Instance names are explicit parameters** (`org`, `name`,
  `repo_ref`), never positional CLI segments.
- **Naming style.** v4 used both `repos_list` (early) and `repo list`
  (current); v5 uses `repos list` (plural noun, space-separated). The
  CLI shape is `synapse -P 44105 lforge-v5 hyperforge <service> <method>`.
- **No `build.*` namespace.** Out of scope for v5 (V5PARITY-9/10/11
  will port what remains useful).

## Behavioral changes worth knowing

- **Soft-delete by default (D12).** `repos.delete` privatizes on every
  remote and marks the record `lifecycle: dismissed`; the local entry
  stays. `repos.purge` is the new hard delete; it requires `dismissed`
  and refuses `protected` records. v4's single-step delete is gone.
- **`create_remote` on `repos.add` is opt-in (D11).** v4's "if it's
  not there, create it" behaviors are explicit: pass `create_remote:
  true` plus `visibility` / `description`.
- **`workspaces.sync` auto-creates absent members (V5PROV-8).** Members
  registered locally but not present on the forge are created (with the
  member's declared `metadata.visibility` / `description`); creation
  failures are reported in the per-repo summary and do NOT abort the
  batch (D6).
- **Push order pinned (D4).** `repos.push` walks remotes in declared
  order; first failure aborts; already-succeeded remotes are surfaced
  in the result.
- **Reconcile ambiguity resolution pinned (D5).** When two local dirs
  share a remote URL, the alphabetically-first dir wins; the rest emit
  `ambiguous` events. No auto-fix.
- **Strong types at the wire boundary.** Newtypes (`OrgName`,
  `RepoName`, `WorkspaceName`, `SecretRef`, ...) replace `String`.
  Closed enums replace stringly-typed providers / lifecycle states.
  Unknown variants are rejected, not silently dropped.

## Why no migration tool

The user explicitly chose to author v5 configs fresh, organisation by
organisation, rather than introduce a backward-compat layer. Reasons:

1. The shape changes (TOML→YAML for orgs, separate→inline repo
   registry, v5-only workspace yamls, narrower per-repo schema) make
   any 1:1 mapping lossy.
2. v4 and v5 run side-by-side without conflict (different ports,
   different registry names, different config file names where they
   differ). Users who depend on v4's build pipeline keep v4 running
   alongside v5 until V5PARITY-9/10/11 land.
3. The ORG → REPO → WORKSPACE re-entry exercise is small (the v5 RPC
   surface is designed for it; see [getting-started](./getting-started.md)).

A future `V5MIGRATE` epic could add migration tooling if real demand
appears. None is on the roadmap.

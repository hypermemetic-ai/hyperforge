# v5 data model

The pinned vocabulary lives in [CONTRACTS §types](../../plans/v5/CONTRACTS.md).
Every type is implemented in [`src/v5/config.rs`](../../src/v5/config.rs)
or [`src/v5/adapters/mod.rs`](../../src/v5/adapters/mod.rs); this doc
just summarises the on-disk + on-wire shapes.

## Relationship sketch

```
~/.config/hyperforge/                                       Per-checkout
├── config.yaml                                             <repo>/.hyperforge/
│     └── provider_map: {<DomainName>: <ProviderKind>}      └── config.toml
│         default_workspace?                                       │
│                                                                  │
├── orgs/<OrgName>.yaml                                            │
│   OrgConfig {                                                    │
│     name: OrgName,                                               │
│     forge: ForgeBlock {                                          │
│       provider: ProviderKind,                                    │
│       credentials: [CredentialEntry { key, type }]               │
│     },                                                           │
│     repos: [OrgRepo {                                            │
│       name: RepoName,                                            │
│       remotes: [Remote { url, provider? }],                      │
│       metadata?: RepoMetadataLocal {                             │
│         default_branch?, description?, archived?,                │
│         visibility?, lifecycle?, privatized_on, protected?       │
│       }                                                          │
│     }]                                                           │
│   }                                                              │
│                                                                  │
├── workspaces/<WorkspaceName>.yaml                                │
│   WorkspaceConfig {                                              │
│     name: WorkspaceName,                                         │
│     path: FsPath,                                                │
│     repos: [WorkspaceRepo:                                       │
│       Shorthand "<org>/<name>"                                   │
│       | Object { ref: RepoRef, dir: String } ]                   │
│   }                                                              │
│                                                                  │
└── secrets.yaml                                                   │
    {<key path>: <value>}                                          │
                          ▲                                        │
                          │                                        │
                          └──── secrets://<path> resolves here ────┘
                                (CredentialEntry.key references)
```

`OrgConfig` owns its repos. `WorkspaceConfig` references them by
`<org>/<name>` ref — the repo registry is single-sourced in the org
yaml. A workspace member that disagrees with its own checkout's
`.hyperforge/config.toml` raises `config_drift` (see D14 below).

## Identifiers

Newtypes over `String`, all `#[serde(transparent)]`:

| Name | Constraint |
|---|---|
| `OrgName` | filename-safe (no `/`, no leading `.`, ≤64 chars, ASCII) |
| `RepoName` | same as `OrgName` |
| `WorkspaceName` | same |
| `RemoteUrl` | any non-empty git URL the `git` CLI accepts |
| `SecretRef` | `secrets://<path>` with non-empty `<path>` |
| `FsPath` | absolute, no `..`, no trailing `/` |
| `DomainName` | lowercase DNS name |

Validation runs at construction sites in the services (e.g.
`is_valid_name`, `is_valid_fspath`); invalid inputs surface as typed
`Error` events.

## Closed enums

| Name | Variants |
|---|---|
| `ProviderKind` | `github`, `codeberg`, `gitlab` |
| `CredentialType` | `token`, `ssh_key` |
| `ProviderVisibility` | `public`, `private`, `internal` (adapters reject variants their provider lacks) |
| `RepoLifecycle` | `active`, `dismissed` (`purged` is transient) |
| `ReconcileEventKind` | `matched`, `renamed`, `removed`, `new_matched`, `ambiguous` |
| `SyncStatus` | `in_sync`, `drifted`, `errored` (workspaces.sync also: `created`) |
| `DriftFieldKind` | `default_branch`, `description`, `archived`, `visibility` |

All serialize as `snake_case` strings. Unknown variants are rejected
at deserialization with `#[serde(deny_unknown_fields)]`.

## Composite shapes

| Type | Field set |
|---|---|
| `RepoRef` | `org: OrgName`, `name: RepoName`. Wire form: object `{org, name}`. YAML accepts both object and shorthand `<org>/<name>` (custom `Deserialize`). |
| `CredentialEntry` | `key: String` (a `secrets://…` ref or absolute path), `type: CredentialType`. |
| `Remote` | `url: RemoteUrl`, optional `provider: ProviderKind` override. |
| `WorkspaceRepo` | `Shorthand("<org>/<name>")` or `Object { reference: RepoRef, dir: String }`. Untagged serde — both shapes parse, source order preserved on round-trip. |
| `OrgRepo` | `name: RepoName`, `remotes: Vec<Remote>`, `metadata: Option<RepoMetadataLocal>`. |
| `RepoMetadataLocal` | `default_branch?`, `description?`, `archived?`, `visibility?`, `lifecycle?: RepoLifecycle`, `privatized_on: BTreeSet<ProviderKind>`, `protected?: bool`. Last three added by V5LIFECYCLE-5. Defaults round-trip absent. |
| `OrgConfig` | `name: OrgName`, `forge: ForgeBlock`, `repos: Vec<OrgRepo>`. |
| `ForgeBlock` | `provider: ProviderKind`, `credentials: Vec<CredentialEntry>`. |
| `WorkspaceConfig` | `name: WorkspaceName`, `path: FsPath`, `repos: Vec<WorkspaceRepo>`. |
| `GlobalConfig` | `default_workspace: Option<WorkspaceName>`, `provider_map: BTreeMap<DomainName, ProviderKind>`. |
| `ForgeMetadata` | The four-field metadata snapshot returned from a forge: `default_branch`, `description`, `archived`, `visibility`. |
| `RemoteRepo` | One repo as surfaced by `ForgePort::list_repos`: `name`, `url`, optional `default_branch / description / archived / visibility`. |

## Round-trip invariants

All YAML files round-trip byte-equivalent through load → save (modulo
key ordering inside `BTreeMap`s, which already serialize sorted). Writes
are atomic (D8): tempfile in the same directory, `fs::rename` at the
end. Concurrent writers serialize per-file inside the daemon.

Lifecycle additions to `RepoMetadataLocal` (`lifecycle`, `protected`,
`privatized_on`) default such that an absent or pre-V5LIFECYCLE-5
metadata block round-trips unchanged. Empty `privatized_on` serializes
as absent.

## D14 — `.hyperforge/config.toml`

A repo checkout MAY carry `.hyperforge/config.toml`. v5's shape is the
deliberately narrow `HyperforgeRepoConfig`:

```toml
repo_name = "widget"
org = "acme"
forges = ["github"]
default_branch = "main"
visibility = "private"        # optional
description = "..."           # optional
```

The file is **written** by `repos.init`. It is **read** by
`workspaces.reconcile` and `workspaces.sync` as a secondary identity
source — primary identity remains the git `origin` URL plus org-yaml
lookup. When the file disagrees with the workspace's assignment, the
**org yaml wins**; reconcile/sync emits a `config_drift` event naming
the discrepancy. No mutation happens from that signal.

The file is intentionally narrower than v4's `HyperforgeConfig` — CI,
distribution, large-file thresholds, SSH key paths, and per-forge
overrides are all v5-out-of-scope. v4-written files carrying those
fields will fail v5's `#[serde(deny_unknown_fields)]` parse. There is
no migration tool by design (see [v4-vs-v5](./v4-vs-v5.md)).

## Secret redaction

`CredentialEntry.key` serializes as-is (the `secrets://…` ref is not a
secret; the resolved value is). No method that returns `OrgDetail` or
any other type containing `CredentialEntry` may include resolved
values. Resolution happens only inside adapters.

The single exception is the `resolve_secret` method on the root service —
which exists for harness reasons (V5CORE-4 acceptance #1) and is
explicitly excluded from the harness's "root-method count" by name.

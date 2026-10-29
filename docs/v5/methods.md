# v5 method reference

Every method below is shipped (its ticket is `status: Complete`).
Methods on Ready/Pending tickets are intentionally absent; they exist
on paper but not in code.

Invoke pattern:

```bash
synapse -P 44105 lforge-v5 hyperforge <service> <method> --<param> <value> ...
```

Every event carries a `type` discriminator (D9). All enum values
serialize `snake_case`. `dry_run` defaults `false` on every mutating
method (D7). YAML writes are atomic (D8).

## Root

Source: [`src/v5/service.rs`](../../src/v5/service.rs).

| Method | Params | Emits |
|---|---|---|
| `status` | — | `status { version, config_dir }` |
| `resolve_secret` | `secret_ref: String` | `secret_resolved { value }` (test-scoped) |

`resolve_secret` is the only wire method that returns plaintext; it
exists for harness reasons (V5CORE-4 acceptance #1). Production callers
use `SecretResolver` directly.

## OrgsService

Source: [`src/v5/orgs.rs`](../../src/v5/orgs.rs).
Files at `<config_dir>/orgs/<name>.yaml`.

| Method | Params | Emits |
|---|---|---|
| `list` | — | one `org_summary { name, provider, repo_count }` per org, ascending by name |
| `get` | `org` | `org_detail { name, provider, credentials[], repos[] }` (`credentials[]` carries refs only — never resolved values) |
| `create` | `name`, `provider`, `dry_run?` | `org_summary` |
| `update` | `org`, `provider?`, `dry_run?` | `org_summary` (rejects no-op call with no changed field) |
| `delete` | `org`, `dry_run?` | `org_deleted { name, dry_run }` (local file only — no forge contact) |
| `set_credential` | `org`, `key`, `credential_type`, `dry_run?` | `credential_added` (new key) or `credential_replaced` (existing key) |
| `remove_credential` | `org`, `key`, `dry_run?` | `credential_removed { org, key, dry_run }` |

`provider` ∈ `{github, codeberg, gitlab}`. `credential_type` ∈
`{token, ssh_key}`. `key` MUST be a `secrets://<path>` ref or an
absolute filesystem path; bare plaintext is rejected.

## ReposService

Source: [`src/v5/repos.rs`](../../src/v5/repos.rs).
Repos live as entries inside `orgs/<org>.yaml`.

### Registry

| Method | Params | Emits |
|---|---|---|
| `forge_port_schema` | — | `forge_port_schema { fields, methods, error_classes }` + `capability` |
| `list` | `org` | one `repo_summary { org, name, remote_count }` per repo |
| `get` | `org`, `name` | `repo_detail { ref, remotes[], metadata? }` |
| `add` | `org`, `name`, `remotes` (JSON array), `create_remote?`, `visibility?`, `description?`, `dry_run?` | `repo_detail` + `repo_added`; with `create_remote=true` also `repo_created` |
| `remove` | `org`, `name`, `delete_remote?`, `dry_run?` | `repo_removed` (local only; `delete_remote=true` returns `unsupported_field`) |
| `add_remote` | `org`, `name`, `remote` (JSON object), `dry_run?` | `repo_detail` |
| `remove_remote` | `org`, `name`, `url`, `dry_run?` | `repo_detail` (rejects removing the last remote) |

`visibility` ∈ `{public, private, internal}`. `internal` is GitLab-only;
GitHub/Codeberg adapters reject it as `unsupported_visibility`.

### Lifecycle (V5LIFECYCLE)

| Method | Params | Emits |
|---|---|---|
| `delete` | `org`, `name`, `dry_run?` | per-remote `forge_privatized` or `privatize_error`; final `repo_dismissed { ref, privatized_on, already }` |
| `purge` | `org`, `name`, `dry_run?` | per-remote `forge_deleted` or `purge_error`; final `repo_purged` (gated on `lifecycle: dismissed`) |
| `protect` | `org`, `name`, `protected`, `dry_run?` | `repo_protection_set { ref, protected }` |
| `init` | `target_path`, `org`, `repo_name`, `forges?`, `default_branch?`, `visibility?`, `description?`, `force?`, `dry_run?` | `hyperforge_config_written { path, repo_name, org }` |

`init` writes `<target_path>/.hyperforge/config.toml`. `target_path` is
named to dodge synapse's path-expansion of any param literally named
`path`. `force=true` overwrites an existing file; the default is to
fail with `already_exists`.

`delete` is a soft-delete (D12): privatize on every remote, set
`lifecycle: dismissed`, keep the local record. `purge` is the hard
delete: requires `dismissed`, calls `adapter.delete_repo` per remote,
then drops the local record. Both refuse `protected` repos.

### Metadata sync (V5REPOS-13/14)

| Method | Params | Emits |
|---|---|---|
| `sync` | `org`, `name`, `remote?` | one `sync_diff { ref, url, status, drift[], remote? }` per remote |
| `push` | `org`, `name`, `remote?`, `fields?`, `dry_run?` | per-remote `push_remote_ok` / `push_remote_error`; final `push_summary { succeeded, errored, aborted }` |

`status` ∈ `{in_sync, drifted, errored}`. `fields` is an optional JSON
object overriding which of `default_branch / description / archived /
visibility` to push; absent → derive from local `metadata:`. First
push failure aborts (D4); already-succeeded remotes are reported.

### Import (V5PARITY-2)

| Method | Params | Emits |
|---|---|---|
| `import` | `org`, `forge?`, `dry_run?` | one `repo_imported { ref, url }` per new repo; final `import_summary { org, total, added, skipped }` |

`forge` (default = the org's declared provider) controls which API
to walk. Already-registered repos are counted as skipped.

### Git transport (V5PARITY-3)

| Method | Params | Emits |
|---|---|---|
| `clone` | `org`, `name`, `dest` | `clone_done { ref, url, dest }` |
| `fetch` | `path`, `remote?` | `fetch_done { ref, remote? }` |
| `pull` | `path`, `remote?`, `branch?` | `pull_done { ref, remote, branch }` (fast-forward only; `dirty_tree` and `non_ff` are typed errors) |
| `push_refs` | `path`, `remote?`, `branch?` | `push_refs_done { ref, remote, branch? }` |
| `status` | `path` | `repo_status { path, branch?, upstream?, ahead, behind, staged, unstaged, untracked, dirty }` |
| `dirty` | `path` | `repo_dirty { path, dirty }` |
| `set_transport` | `org`, `name`, `transport` (`ssh`\|`https`), `path?` | `transport_set { ref, transport }` |

These shell out to the user's `git` CLI through `ops::git` so SSH
agents, credential helpers, and hooks all flow through unchanged.

## WorkspacesService

Source: [`src/v5/workspaces.rs`](../../src/v5/workspaces.rs).
Files at `<config_dir>/workspaces/<name>.yaml`.

### CRUD (V5WS-2..7)

| Method | Params | Emits |
|---|---|---|
| `list` | — | one `workspace_summary { name, path, repo_count }` per workspace |
| `get` | `name` | `workspace_detail { name, path, repos[] }` (preserves on-disk shape: shorthand or `{ref, dir}` object) |
| `create` | `name`, `ws_path`, `repos?` (JSON array), `dry_run?` | `workspace_summary`. Every repo ref is validated against its `orgs/<org>.yaml`; unresolved refs abort the write |
| `delete` | `name`, `delete_remote?`, `dry_run?` | `workspace_deleted { name }`; cascade emits `forge_delete_result` per member (cascade has no adapter path in v1) |
| `add_repo` | `name`, `repo_ref` (`<org>/<name>` or `{org,name}`), `dry_run?` | `workspace_summary` (rejects already-member, unresolved org/repo) |
| `remove_repo` | `name`, `repo_ref`, `delete_remote?`, `dry_run?` | `workspace_summary`; cascade emits `forge_delete_result` per cascade |

`ws_path` (not `path`) is the workspace directory. Same path-expansion
dodge as `repos.init --target_path`.

### Reconcile + sync (V5WS-8/9 + V5LIFECYCLE-10 + V5PROV-8)

| Method | Params | Emits |
|---|---|---|
| `reconcile` | `name`, `dry_run?` | one `reconcile_event { kind, ref?, dir?, detail? }` per observation; `config_drift` per `.hyperforge/config.toml` mismatch |
| `sync` | `name`, `include_dismissed?` | one `sync_diff` per member; `sync_skipped` per dismissed member; `config_drift` per identity mismatch; final `workspace_sync_report { name, total, in_sync, drifted, errored, created, skipped, per_repo }` |

`reconcile_event.kind` ∈ `{matched, renamed, removed, new_matched,
ambiguous}`. Reconcile is read-only against forges; it only rewrites
the workspace yaml on `renamed` / `removed` decisions (and only when
not `dry_run`). Under D5, when multiple dirs match one ref, the
alphabetically-first wins; the rest emit `ambiguous`.

`sync` continues past per-member failures (D6). Members registered
locally but absent on the remote are auto-created via
`adapter.create_repo` (V5PROV-8). `lifecycle: dismissed` members are
skipped by default; `include_dismissed=true` overrides.

Invariant: `total == in_sync + drifted + errored + created + skipped`.

### Discover (V5PARITY-2)

| Method | Params | Emits |
|---|---|---|
| `discover` | `path`, `name?`, `dry_run?` | one `discover_match { dir, status, ref?, origin? }` per scanned dir; final `workspace_discovered { name, path, repo_count }` |

`status` ∈ `{matched, orphan, already_member}`. Walks every subdir of
`path`, reads each `.git/config` for the `origin` URL, and matches
against every org's known remotes. Matched dirs that aren't already a
member of any workspace are written into a new (or existing) workspace
yaml at `<config_dir>/workspaces/<name>.yaml`.

### Workspace-parallel git ops (V5PARITY-3)

| Method | Params | Emits |
|---|---|---|
| `clone` | `name` | per-member `member_git_result {op: "clone", status, message?}`; final `workspace_git_summary { name, op, total, ok, errored }` |
| `fetch` | `name` | same shape, `op: "fetch"` |
| `pull` | `name` | same shape, `op: "pull"` (fast-forward only) |
| `push_all` | `name` | same shape, `op: "push_refs"` |

Sequential in v1; bounded parallelism is deferred to V5PARITY-12.

## Standard event envelopes

All events carry `type: <snake_case>`. Errors use:

```json
{"type": "error", "code": "<snake_case>", "message": "<free text>"}
```

Repos events additionally carry `error_class` (drawn from the
`ForgePort` closed set: `not_found`, `auth`, `network`,
`unsupported_field`, `rate_limited`, `conflict`,
`unsupported_visibility`).

## Closed enum vocabularies

| Name | Variants | Source |
|---|---|---|
| `ProviderKind` | `github`, `codeberg`, `gitlab` | CONTRACTS §types |
| `CredentialType` | `token`, `ssh_key` | CONTRACTS §types |
| `ProviderVisibility` | `public`, `private`, `internal` | CONTRACTS §types |
| `RepoLifecycle` | `active`, `dismissed` (`purged` is transient — purged records are deleted) | CONTRACTS §types + D12 |
| `ReconcileEventKind` | `matched`, `renamed`, `removed`, `new_matched`, `ambiguous` | CONTRACTS §types + D5 |
| `SyncStatus` (per-remote) | `in_sync`, `drifted`, `errored`; workspaces.sync also emits `created` | CONTRACTS §types + V5PROV-8 |
| `DriftFieldKind` | `default_branch`, `description`, `archived`, `visibility` | D3 / D10 |

Unknown variants are rejected at the wire boundary (`#[serde(deny_unknown_fields)]` /
closed enum), not silently dropped.

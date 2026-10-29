# Getting started with hyperforge v5

This walks the end-to-end flow: install the daemon, add an org, set a
credential, create a workspace, then import / clone / sync. Every
command runs against a real `hyperforge-v5` daemon via `synapse`.

## Install

v5 ships as the `hyperforge-v5` binary out of the same crate as v4. Build
it from a checkout:

```bash
cd ~/dev/controlflow/hypermemetic/hyperforge
cargo build --bin hyperforge-v5 --release
```

Output lands at `target/release/hyperforge-v5`. Drop it on `$PATH` (or
launch by absolute path).

## Run the daemon

```bash
hyperforge-v5 --port 44105 --config-dir ~/.config/hyperforge
```

- `--port` defaults to 44105 (D1). v5's registry namespace is
  `lforge-v5`, distinct from v4's `lforge`.
- `--config-dir` defaults to `~/.config/hyperforge/`. The directory is
  created if missing. `~` is expanded.

The daemon is a single binary — no `hyperforge-auth` sidecar (D2).
Secrets resolve through an embedded YAML store at
`<config_dir>/secrets.yaml`.

## Smoke check

```bash
synapse -P 44105 lforge-v5 hyperforge status
```

Returns `{type: "status", version: "<crate version>", config_dir: "<path>"}`.

## Add an org

An org pins one provider (`github`, `codeberg`, or `gitlab`) and owns
both credentials and the repo registry under that provider.

```bash
synapse -P 44105 lforge-v5 hyperforge orgs create \
    --name acme \
    --provider github
```

Result: `~/.config/hyperforge/orgs/acme.yaml` with an empty `credentials`
list and an empty `repos` list. `dry_run=true` previews without writing.

To list / inspect:

```bash
synapse -P 44105 lforge-v5 hyperforge orgs list
synapse -P 44105 lforge-v5 hyperforge orgs get --org acme
```

## Set a credential

Tokens (and SSH key paths) live in `secrets.yaml` and are referenced from
the org's `credentials[]` list by `secrets://<path>` ref. There is no
`hyperforge-auth` for v5; you write `secrets.yaml` with whatever editor
you like, then point the org at the ref.

Step 1: store the token. The simplest path is a one-line YAML edit:

```yaml
# ~/.config/hyperforge/secrets.yaml
github/acme/token: "ghp_..."
```

(`secrets.set` lands in V5PARITY-7. Until then, hand-edit the file.)

Step 2: register the ref against the org:

```bash
synapse -P 44105 lforge-v5 hyperforge orgs set_credential \
    --org acme \
    --key 'secrets://github/acme/token' \
    --credential_type token
```

The org yaml now lists the ref. Resolution happens inside adapters;
no method that returns an `OrgDetail` ever surfaces the plaintext.

## Add a repo

```bash
synapse -P 44105 lforge-v5 hyperforge repos add \
    --org acme \
    --name widget \
    --remotes '[{"url":"https://github.com/acme/widget.git"}]'
```

`remotes` is a JSON array. Each entry is `{url: "..."}` plus an optional
`provider:` override; without it, the URL's domain is matched against
`config.yaml`'s `provider_map` to derive the provider.

To also create the remote on the forge in the same call:

```bash
synapse -P 44105 lforge-v5 hyperforge repos add \
    --org acme \
    --name widget \
    --remotes '[{"url":"https://github.com/acme/widget.git"}]' \
    --create_remote true \
    --visibility private \
    --description "Demo repo"
```

This calls `adapter.repo_exists` (conflict if it already does), then
`adapter.create_repo`. On forge error, the local entry is rolled back.

## Import existing repos from a forge

`repos.import` walks the forge's API and registers every repo it finds
that isn't already in the org yaml.

```bash
synapse -P 44105 lforge-v5 hyperforge repos import --org acme
# or scope to a specific forge:
synapse -P 44105 lforge-v5 hyperforge repos import --org acme --forge github
```

Each newly registered repo emits `repo_imported`; an `import_summary`
event closes the stream with `total / added / skipped` counts.

## Create a workspace

A workspace declares a directory and the repos that should live inside
it. Each member is an `<org>/<name>` ref.

```bash
synapse -P 44105 lforge-v5 hyperforge workspaces create \
    --name main \
    --ws_path /home/me/work \
    --repos '["acme/widget"]'
```

The parameter is `ws_path`, not `path` — synapse path-expands any
parameter literally named `path`, so v5 renames the user-facing arg
to dodge that.

Cloning every member into the workspace dir:

```bash
synapse -P 44105 lforge-v5 hyperforge workspaces clone --name main
```

Each member emits `member_git_result {op: "clone", status: "ok"|"errored"}`,
followed by a `workspace_git_summary` aggregate.

## Sync metadata

`repos.sync` reads each remote's metadata (default branch, description,
archived, visibility) and emits a `sync_diff` per remote. Drift is the
fields where local declared metadata disagrees with the remote.

```bash
synapse -P 44105 lforge-v5 hyperforge repos sync --org acme --name widget
```

For workspace-wide sync:

```bash
synapse -P 44105 lforge-v5 hyperforge workspaces sync --name main
```

Per-member `sync_diff` events stream out, then a final
`workspace_sync_report { total, in_sync, drifted, errored, created, skipped }`.

If a member is registered locally but absent on the forge (and not
dismissed), `workspaces.sync` calls `adapter.create_repo` for it. Members
in `lifecycle: dismissed` are skipped unless `include_dismissed=true`.

## Push declared metadata

```bash
synapse -P 44105 lforge-v5 hyperforge repos push --org acme --name widget
```

Pushes every field declared in the local `metadata:` block to every
remote, in sequence. First failure aborts (D4); the `push_summary` at
the end lists succeeded + errored remotes.

## Delete and purge

`repos.delete` is a soft delete (D12): it sets the remote to private on
every forge, marks the local record `lifecycle: dismissed`, and keeps
it in `orgs/<org>.yaml` for audit. The forge repo still exists.

```bash
synapse -P 44105 lforge-v5 hyperforge repos delete --org acme --name widget
```

`repos.purge` is the hard delete: gated on `dismissed`, calls
`adapter.delete_repo` per remote, then drops the local record.

```bash
synapse -P 44105 lforge-v5 hyperforge repos purge --org acme --name widget
```

`repos.protect --protected true` sets a guard bit; both delete and purge
refuse a protected repo until you flip it back.

## Where to go next

- [Methods reference](./methods.md) for the full RPC surface.
- [Architecture](./architecture.md) for what happens inside a service
  method.

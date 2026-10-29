# CONF-5: Config-First Repo Service Operations

**blocked_by**: [CONF-3]
**unlocks**: [CONF-7]

## Scope

Rewire `RepoService` methods so all mutations flow through LocalForge first, then materialize to disk. Currently, `init` writes config directly, `create` writes LocalForge but not config, `rename` updates LocalForge+remotes but not config, etc. After this ticket, the pattern is always: mutate LocalForge → save → materialize.

## Method-by-Method Changes

### `repo.create(org, name, ...)` — currently: LocalForge only

**Before**: Writes to LocalForge + saves YAML. No per-repo config or git remotes.

**After**: Same LocalForge write. If `local_path` is provided (or discoverable), call `materialize(org, record, path, default_opts)` to project config+remotes. If no local path, skip materialization (repo may not be cloned yet).

### `repo.update(org, name, ...)` — currently: LocalForge only

**Before**: Updates description/visibility in LocalForge. No per-repo config update.

**After**: Same LocalForge mutation. If record has `local_path`, call `materialize()` to update the on-disk config. This ensures description/visibility changes propagate to `.hyperforge/config.toml`.

### `repo.init(path, forges, org, ...)` — currently: config+remotes only, no LocalForge

**Before**: Creates `.hyperforge/config.toml` and adds git remotes. Does NOT register in LocalForge.

**After**:
1. Build a `RepoRecord` from the provided params (forges, org, visibility, etc.)
2. Set `record.local_path = Some(path)`
3. `state.get_local_forge(org).upsert_record(record)` + `save_to_yaml()`
4. `materialize(org, &record, path, opts_with_hooks_and_ssh)`

This closes Disconnect #1 (init doesn't register in LocalForge).

### `repo.delete(org, name)` — currently: privatize remotes + mark dismissed

**Before**: Privatizes on forges, marks dismissed in LocalForge. Per-repo config untouched.

**After**: Same forge privatization + LocalForge dismissed flag. Additionally, if record has `local_path` and the path exists, update per-repo config to reflect dismissed state (or leave config as-is — deletion is a registry concern, not a config concern). No change strictly needed here, but document the decision.

### `repo.rename(org, old, new)` — currently: renames on forges + LocalForge, not config

**Before**: Renames on remote forges, updates LocalForge. Per-repo config files in clones have stale `repo_name`.

**After**: Same forge rename + LocalForge rename. If record has `local_path`, call `materialize()` to rewrite config with new name. Also update git remote URLs via materialize's remote reconciliation (URLs contain the repo name).

This closes Disconnect #5 (rename doesn't update per-repo config).

### `repo.import(org, forge)` — currently: LocalForge only

**Before**: Fetches from remote forge, populates LocalForge. No per-repo config anywhere.

**After**: Same behavior. Import is a registry operation — repos aren't cloned locally, so there's nothing to materialize. No change needed. (Clone + materialize happens via `repo.clone` or `workspace.clone`.)

### `repo.clone(org, name, path)` — currently: clones git, no config

**Before**: Looks up in LocalForge, clones git repo, adds mirror remotes. Does NOT create `.hyperforge/config.toml`.

**After**: Same clone. Then:
1. Set `record.local_path = Some(clone_path)` in LocalForge
2. `materialize(org, &record, clone_path, default_opts)` — creates config + reconciles remotes

This closes Disconnect #7 (clone doesn't create config).

## Acceptance Criteria

- [ ] `repo.init` registers in LocalForge before materializing
- [ ] `repo.create` materializes if local_path is known
- [ ] `repo.update` materializes if local_path is known
- [ ] `repo.rename` materializes after rename to update config + remotes
- [ ] `repo.clone` materializes after cloning
- [ ] All methods save LocalForge YAML after mutation
- [ ] No direct `HyperforgeConfig::save()` calls remain in repo.rs (all go through materialize)
- [ ] Existing functionality preserved — same events, same forge API behavior
- [ ] `cargo build --release` succeeds
- [ ] Existing tests pass

## Notes

- `repo.delete` and `repo.purge` are debatable — do we materialize a "deleted" state to disk? Probably not. Deletion is a registry+forge concern. The local clone becomes orphaned, which is fine.
- `repo.import` doesn't materialize because there's no local clone. This is correct.
- `repo.status` and `repo.push` are read-only / action-only, no config mutations needed.

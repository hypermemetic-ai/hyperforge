# CONF-7: Cleanup, Migration, Dead Code Removal

**blocked_by**: [CONF-5, CONF-6]
**unlocks**: []

## Scope

After all operations flow through LocalForge → materialize, clean up the codebase:

1. Remove direct `HyperforgeConfig::save()` calls outside of `materialize()`
2. Remove duplicated init/config logic that materialize replaces
3. Simplify `HyperforgeConfig` — it becomes a serialization type, not a source of truth
4. Add a migration path for existing repos.yaml files (new fields)
5. Update documentation

## Changes

### 1. Audit and remove direct config writes

After CONF-5 and CONF-6, no code outside `materialize()` should call `HyperforgeConfig::save()`. Find and remove any remaining calls:

- `src/commands/init.rs` — `config.save()` call replaced by materialize in CONF-5
- `src/services/workspace.rs` — any remaining `config.save()` in move_repos replaced by materialize in CONF-6
- `src/services/repo.rs` — any remaining direct config writes replaced in CONF-5

If `init.rs` is only used for its hook installation and SSH wrapper logic (now extracted into materialize), consider removing `init.rs` entirely or reducing it to the hook/SSH helpers that materialize calls.

### 2. Simplify `HyperforgeConfig`

`HyperforgeConfig` becomes purely a serialization format for `.hyperforge/config.toml`. It no longer needs:
- Builder methods for constructing configs from scratch (materialize builds from RepoRecord)
- `parse_forge()` — move to a shared util if still needed

Keep: `load()`, `save()`, `Serialize`/`Deserialize` derives, field definitions.

### 3. Remove `repo_from_config()` bridge

`src/commands/workspace.rs::repo_from_config()` converts `DiscoveredRepo` → `Repo` for LocalForge registration. After CONF-2's `merge_from_config()`, this conversion happens at the `RepoRecord` level. Remove `repo_from_config()` and update callers to use `RepoRecord::merge_from_config()`.

### 4. Clean up `discover_workspace()`

After config-first, `discover_workspace()` is still needed for bootstrapping (finding repos on disk). But its `DiscoveredRepo.config` field is now only used for `merge_from_config()` during registration. Verify the type still makes sense.

### 5. Migration for existing repos.yaml

Old `repos.yaml` files lack the new CONF-2 fields (`forges`, `ssh`, `forge_config`, `ci`, `local_path`). Serde defaults handle deserialization, but the records will have empty fields until a sync populates them via `merge_from_config()`.

Add a one-time migration step to `workspace.sync`:
- After Phase 1 discover, for each discovered repo that has a LocalForge record with empty `forges`:
  - Load per-repo config from disk
  - Call `record.merge_from_config(&config)`
  - Save updated record

This populates the new fields from existing per-repo configs. After one sync, LocalForge is complete.

### 6. Update CLAUDE.md

Update the Hyperforge section in CLAUDE.md to reflect:
- LocalForge is the single source of truth
- Per-repo config is a projection via `materialize()`
- `init` now registers in LocalForge
- `clone` now creates per-repo config
- New `materialize` command/concept

## Acceptance Criteria

- [ ] No direct `HyperforgeConfig::save()` calls outside `materialize()`
- [ ] `repo_from_config()` removed, callers use `merge_from_config()`
- [ ] `init.rs` simplified or reduced to hook/SSH helpers
- [ ] Migration step populates new RepoRecord fields from existing per-repo configs
- [ ] CLAUDE.md updated with config-first architecture description
- [ ] No dead code warnings
- [ ] `cargo build --release` succeeds
- [ ] Existing tests pass
- [ ] Manual smoke test: fresh workspace → init → sync → verify config+registry+remotes all consistent

## Notes

- This is the "polish" ticket. It should be relatively small if CONF-5 and CONF-6 are done well.
- The migration step is important for existing users — their repos.yaml files will work without it, but the new fields won't be populated until they run sync.
- Consider adding a `hyperforge migrate` command that runs the migration explicitly, for users who want to upgrade without a full sync.

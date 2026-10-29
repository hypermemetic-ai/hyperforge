# WORK-5: Retrofit All Commands

blocked_by: [WORK-4]
unlocks: []

## Scope

Make `--path` optional and add `--workspace` to every workspace/build command. Commands aggregate results across workspaces when operating on multiple.

## Affected Methods

### WorkspaceService: sync, push_all, diff, check, status, pull, verify, verify_sync, clone, move_repos, init_configs, set_default_branch, update_ssh
### BuildService: dirty, activity, loc, large_files, analyze, package_diff, run, exec, release, release_all, binstall_init, dist_show, dist_init, brew_formula

## Pattern

Every method changes from:
```rust
pub async fn dirty(path: String, ...) -> impl Stream
```
to:
```rust
pub async fn dirty(path: Option<String>, workspace: Option<String>, ...) -> impl Stream
```

Then uses `resolve_workspaces` at the top.

## Tests

### `test_dirty_single_workspace`
Register workspace "alpha" pointing at a tempdir with 2 repos (one dirty, one clean).
Call `build dirty --workspace alpha`. Assert output shows 1 dirty, 1 clean.

### `test_dirty_all_workspaces`
Register "alpha" (1 dirty repo) and "beta" (0 dirty repos).
Call `build dirty` (no args). Assert output contains:
- A section for "alpha" with 1 dirty
- A section for "beta" with 0 dirty
- Or an aggregated summary line

### `test_dirty_path_still_works`
Don't register any workspace. Call `build dirty --path /tmp/myws`.
Assert it works exactly as before (backward compat).

### `test_sync_all_workspaces`
Register "alpha" and "beta". Call `workspace sync --dry_run true` (no --path, no --workspace).
Assert output contains Phase lines for both workspaces.
Assert each workspace's output is prefixed with the workspace name.

### `test_sync_single_workspace`
Register "alpha" and "beta". Call `workspace sync --workspace alpha --dry_run true`.
Assert output only contains "alpha" phases, not "beta".

### `test_release_all_multi_workspace`
Register "alpha" (has a Rust binary) and "beta" (has a Haskell binary).
Call `build release_all --tag v1.0.0 --dry_run true` (no --path).
Assert output shows dry-run release for both workspaces.

### `test_path_and_workspace_errors`
Call `build dirty --path /tmp/x --workspace alpha`.
Assert error: "Cannot specify both --path and --workspace".

### `test_no_args_no_workspaces_errors`
Empty registry. Call `build dirty` (no --path, no --workspace).
Assert error containing "No workspaces registered".

### `test_output_prefixed_when_multi`
Register 2 workspaces. Call `build loc` (no args).
Assert each event or output line is prefixed with `[workspace_name]`.

### `test_output_not_prefixed_when_single`
Register 1 workspace. Call `build loc` (no args).
Assert output is NOT prefixed (single workspace = no visual noise).

### `test_activity_aggregated`
Register 2 workspaces. Call `build activity` (no args).
Assert output contains repo activity from both workspaces.

### Service method registration test

### `test_build_service_methods_have_workspace_param`
Fetch the BuildService schema. For every method that previously took `path`, assert the schema now includes both `path` (optional) and `workspace` (optional) params.

### `test_workspace_service_methods_have_workspace_param`
Same for WorkspaceService methods.

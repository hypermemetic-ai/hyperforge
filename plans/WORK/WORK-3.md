# WORK-3: Workspace Register/Unregister/List Commands

blocked_by: [WORK-2]
unlocks: [WORK-7]

## Scope

CRUD commands for the workspace registry, exposed as service methods.

## Methods

### `workspace_register`
Params: `name` (String), `path` (String), `org` (String), `forges` (Option<Vec<Forge>>)

### `workspace_unregister`
Params: `name` (String), `confirm` (Option<bool>, default false = dry-run)

### `workspace_list`
No required params.

## Tests

### Integration tests (against running service, or via direct method call)

### `test_register_emits_success`
Call `workspace_register(name="testws", path="/tmp/testws", org="testorg", forges=Some(vec![GitHub]))`.
Collect events. Assert at least one Info event contains "Registered workspace 'testws'".
Call `workspace_list`, collect events. Assert one event contains "testws" and "/tmp/testws".

### `test_register_invalid_path`
Call `workspace_register` with a path that doesn't exist (`/nonexistent/path`).
Assert an event contains a warning about the path not existing.
Assert it still registers (warn but don't block — path may be created later).

### `test_register_duplicate_name`
Register "foo" with path A. Register "foo" again with path B.
Assert the second call emits a warning about overwriting.
Call `workspace_list`. Assert "foo" has path B.

### `test_unregister_dry_run`
Register "foo". Call `workspace_unregister(name="foo")` (no confirm).
Assert event says "[dry-run] Would unregister workspace 'foo'".
Call `workspace_list`. Assert "foo" is still present.

### `test_unregister_confirmed`
Register "foo". Call `workspace_unregister(name="foo", confirm=true)`.
Assert event says "Unregistered workspace 'foo'".
Call `workspace_list`. Assert no events (empty).

### `test_unregister_missing`
Call `workspace_unregister(name="nonexistent", confirm=true)`.
Assert error event: "Workspace 'nonexistent' not found".

### `test_list_empty`
With no workspaces registered, call `workspace_list`.
Assert single Info event: "No workspaces registered."

### `test_list_multiple`
Register 3 workspaces with different names/paths/orgs.
Call `workspace_list`. Collect all events.
Assert 3 events, each containing the name, path, and org of one workspace.

### `test_list_shows_repo_count`
Register a workspace pointing at a real tempdir with 3 subdirectories containing `.git/`.
Call `workspace_list`. Assert the event for that workspace contains "3 repos".

### `test_persistence_across_reload`
Register a workspace. Reload the registry from disk (simulate restart).
Call `workspace_list`. Assert the workspace is still present.

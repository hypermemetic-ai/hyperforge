# LFORGE-9: Cleanup Old Code

**blocked_by:** [LFORGE-8]
**unlocks:** []

## Scope

Remove deprecated code that has been replaced by the LocalForge architecture. This includes `StoragePort`, `ImportService`, legacy state types, and storage adapters. Only proceed after LFORGE-8 verification gate passes.

## Deliverables

1. Remove `StoragePort` trait and implementations
2. Remove `ImportService`
3. Remove `DesiredState`, `ObservedState`, `SyncedState` wrappers
4. Remove `yaml_storage.rs` adapter
5. Remove `memory_storage.rs` adapter
6. Remove deprecated type aliases
7. Update all imports and exports
8. Clean up unused dependencies

## Verification Steps

```bash
cd ~/dev/controlflow/hypermemetic/hyperforge

# Ensure all tests still pass after removal
cargo test

# No compilation errors
cargo build

# No unused code warnings
cargo build 2>&1 | grep "unused"
# Should be minimal/none

# Documentation still builds
cargo doc

# Check for any remaining references to removed items
grep -r "StoragePort" src/
grep -r "ImportService" src/
grep -r "DesiredState" src/
grep -r "SyncedState" src/
# All should return nothing
```

## Implementation Notes

### Files to Remove

```bash
# Remove these files entirely:
rm src/ports/storage.rs
rm src/adapters/yaml_storage.rs
rm src/adapters/memory_storage.rs
rm src/services/import.rs

# If DesiredState/SyncedState are in separate files:
rm src/domain/desired_state.rs  # if exists
rm src/domain/synced_state.rs   # if exists
```

### Files to Update

#### `src/ports/mod.rs`

```rust
// REMOVE these lines:
// mod storage;
// pub use storage::{StoragePort, StorageError};

// KEEP:
mod forge;
mod secrets;

pub use forge::{ForgePort, ForgeError};
pub use secrets::{SecretsPort, SecretsError};
```

#### `src/adapters/mod.rs`

```rust
// REMOVE these lines:
// mod yaml_storage;
// mod memory_storage;
// pub use yaml_storage::YamlStorageAdapter;
// pub use memory_storage::InMemoryStorageAdapter;

// KEEP:
mod local_forge;
mod github;
mod codeberg;

pub use local_forge::LocalForge;
pub use github::GitHubAdapter;
pub use codeberg::CodebergAdapter;
```

#### `src/services/mod.rs`

```rust
// REMOVE these lines:
// mod import;
// pub use import::ImportService;

// KEEP/UPDATE:
mod symmetric_sync;
mod diff;  // Keep if still used, or merge into symmetric_sync

pub use symmetric_sync::{SymmetricSyncService, SyncOptions, SyncReport, SyncOutcome};
```

#### `src/domain/mod.rs`

```rust
// REMOVE deprecated re-exports:
// #[deprecated]
// pub use desired::DesiredRepo;
// #[deprecated]
// pub use observed::ObservedRepo;

// KEEP the new types:
mod repo;
mod sync_action;
mod identity;
mod diff;

pub use repo::Repo;
pub use sync_action::{SyncAction, PropertyDiff};
pub use identity::RepoIdentity;
pub use diff::compute_sync_actions;

// Optionally keep old types without deprecation if still needed internally
// Or remove entirely if all code migrated
```

#### `src/lib.rs`

```rust
// Update public exports
pub mod adapters;
pub mod domain;
pub mod ports;
pub mod services;
pub mod types;

// Remove any re-exports of deleted items
```

### Old SyncService Updates

If keeping the old sync service temporarily:

```rust
// In src/services/sync.rs

// Remove StoragePort dependency
pub struct SyncService {
    forges: Vec<Arc<dyn ForgePort>>,
    // REMOVE: storage: Arc<dyn StoragePort>,
}

impl SyncService {
    // Update constructor
    pub fn new(forges: Vec<Arc<dyn ForgePort>>) -> Self {
        Self { forges }
    }

    // Remove methods that used StoragePort
    // Or deprecate entire struct in favor of SymmetricSyncService
}
```

### Dependency Cleanup

Check `Cargo.toml` for unused dependencies:

```toml
# These might be removable if only used by deleted code:
# - Check if serde_yaml is still needed (yes, for LocalForge persistence)
# - Check other deps
```

```bash
# Find unused dependencies
cargo +nightly udeps
```

### Migration Guide for External Code

If there are external consumers of the removed APIs:

```rust
// Old code:
let storage = YamlStorageAdapter::new(path);
let desired = storage.load_desired("org").await?;

// New code:
let local = LocalForge::load(path)?;
let repos = local.list_repos("org").await?;
let repos: Vec<Repo> = repos.into_iter().map(Repo::from).collect();
```

```rust
// Old code:
let import_service = ImportService::new(forge, storage);
import_service.import("org").await?;

// New code:
let remote = get_github_forge();
let local = LocalForge::load_or_create(path)?;
SymmetricSyncService::sync(&remote, &local, "org", SyncOptions::new()).await?;
```

### Checklist

Before removing each item, verify:

- [ ] No compile errors when removed
- [ ] All tests still pass
- [ ] No runtime errors in integration tests
- [ ] Documentation updated

#### StoragePort Removal

- [ ] Remove `src/ports/storage.rs`
- [ ] Remove from `src/ports/mod.rs`
- [ ] Update all imports
- [ ] Tests pass

#### ImportService Removal

- [ ] Remove `src/services/import.rs`
- [ ] Remove from `src/services/mod.rs`
- [ ] Update service/activations to use SymmetricSyncService
- [ ] Tests pass

#### YamlStorageAdapter Removal

- [ ] Remove `src/adapters/yaml_storage.rs`
- [ ] Remove from `src/adapters/mod.rs`
- [ ] Verify LocalForge handles all YAML persistence
- [ ] Tests pass

#### InMemoryStorageAdapter Removal

- [ ] Remove `src/adapters/memory_storage.rs`
- [ ] Remove from `src/adapters/mod.rs`
- [ ] Verify all tests use LocalForge instead
- [ ] Tests pass

#### Legacy Domain Types

- [ ] Remove `DesiredState` wrapper (if exists)
- [ ] Remove `ObservedState` wrapper (if exists)
- [ ] Remove `SyncedState` wrapper (if exists)
- [ ] Keep `DesiredRepo`/`ObservedRepo` if still needed, or remove
- [ ] Update domain/mod.rs exports
- [ ] Tests pass

### Final Verification

```bash
# Complete test suite
cargo test

# Build in release mode
cargo build --release

# Check for dead code
cargo build 2>&1 | grep -E "(unused|dead_code)"

# Verify public API
cargo doc --open
# Check that only intended items are exported

# Run clippy
cargo clippy -- -D warnings
```

### Rollback Plan

If issues are discovered after removal:

1. All removed code is still in git history
2. Can revert specific commits
3. Or restore files from `git show <commit>:path/to/file`

```bash
# To restore a deleted file:
git show HEAD~1:src/ports/storage.rs > src/ports/storage.rs
```

### Key Design Decisions

1. **Remove, don't deprecate**: Once LFORGE-8 passes, fully remove old code
2. **One PR per major removal**: Easier to revert if needed
3. **Test after each removal**: Don't batch deletions
4. **Keep git history**: Don't squash commits, preserve ability to restore
5. **Document breaking changes**: Update CHANGELOG if this is a library

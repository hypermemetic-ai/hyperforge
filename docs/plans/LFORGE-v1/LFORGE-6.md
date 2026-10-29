# LFORGE-6: Update Activations

**blocked_by:** [LFORGE-3, LFORGE-4, LFORGE-5]
**unlocks:** [LFORGE-7, LFORGE-8]

## Scope

Update the activation layer to use the new symmetric sync architecture. `WorkspaceActivation` will use `LocalForge` for state management. `OrgActivation.import` becomes `sync(remote, local)`. `ReposActivation.sync` becomes `sync(local, remote)`. This completes the integration of LocalForge into the system.

## Deliverables

1. Updated `WorkspaceActivation` using LocalForge for local state
2. Updated `OrgActivation` with import as `sync(remote, local)`
3. Updated `ReposActivation` with sync as `sync(local, remote)`
4. Remove direct StoragePort usage from activations
5. Integration tests verifying end-to-end flow

## Verification Steps

```bash
# Run activation tests
cd ~/dev/controlflow/hypermemetic/hyperforge
cargo test activations::

# Test import command
cargo test test_org_import_uses_symmetric_sync

# Test sync command
cargo test test_repos_sync_uses_symmetric_sync

# Integration test
cargo test test_import_then_sync_roundtrip
```

## Implementation Notes

### WorkspaceActivation Updates

Update `src/activations/workspace/activation.rs`:

```rust
use crate::adapters::LocalForge;
use crate::services::SymmetricSyncService;

pub struct WorkspaceActivation {
    /// Local forge for storing desired state
    local_forge: LocalForge,
    /// Path for persistence
    config_path: PathBuf,
}

impl WorkspaceActivation {
    pub fn new(config_path: PathBuf) -> Result<Self, WorkspaceError> {
        let local_forge = LocalForge::load_or_create(&config_path)
            .map_err(|e| WorkspaceError::ConfigError(e.to_string()))?;

        Ok(Self {
            local_forge,
            config_path,
        })
    }

    /// Get reference to local forge for sync operations
    pub fn local_forge(&self) -> &LocalForge {
        &self.local_forge
    }

    /// Save current state to disk
    pub fn save(&self, org: &str) -> Result<(), WorkspaceError> {
        self.local_forge.save(&self.config_path, org)
            .map_err(|e| WorkspaceError::ConfigError(e.to_string()))
    }
}
```

### OrgActivation Import Updates

Update `src/activations/org/activation.rs`:

```rust
use crate::services::{SymmetricSyncService, SyncOptions, SyncReport};

impl OrgActivation {
    /// Import repositories from remote forge to local state.
    ///
    /// This is symmetric sync: `sync(remote_forge, local_forge)`
    pub async fn import(
        &self,
        remote_forge: &dyn ForgePort,
        local_forge: &LocalForge,
        org: &str,
        options: ImportOptions,
    ) -> Result<SyncReport, OrgError> {
        let sync_options = SyncOptions {
            dry_run: options.dry_run,
            delete_missing: false, // Import doesn't delete local repos
            repo_filter: options.repo_filter,
        };

        SymmetricSyncService::sync(
            remote_forge,  // source
            local_forge,   // target
            org,
            sync_options,
        ).await.map_err(|e| OrgError::SyncError(e.to_string()))
    }
}

/// Options for import operation
#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    pub dry_run: bool,
    pub repo_filter: Option<HashSet<String>>,
    pub include_private: bool,
}
```

### ReposActivation Sync Updates

Update `src/activations/repos/activation.rs`:

```rust
use crate::services::{SymmetricSyncService, SyncOptions, SyncReport};

impl ReposActivation {
    /// Sync repositories from local state to remote forge.
    ///
    /// This is symmetric sync: `sync(local_forge, remote_forge)`
    pub async fn sync(
        &self,
        local_forge: &LocalForge,
        remote_forge: &dyn ForgePort,
        org: &str,
        options: SyncRepoOptions,
    ) -> Result<SyncReport, ReposError> {
        let sync_options = SyncOptions {
            dry_run: options.dry_run,
            delete_missing: options.delete_missing,
            repo_filter: options.repo_filter,
        };

        SymmetricSyncService::sync(
            local_forge,    // source
            remote_forge,   // target
            org,
            sync_options,
        ).await.map_err(|e| ReposError::SyncError(e.to_string()))
    }

    /// Create a new repository in local state.
    ///
    /// After calling this, use sync() to push to remote forges.
    pub async fn create(
        &self,
        local_forge: &LocalForge,
        org: &str,
        name: &str,
        options: CreateRepoOptions,
    ) -> Result<(), ReposError> {
        let desired = DesiredRepo::new(
            RepoIdentity::new(org, name),
            options.visibility,
            HashSet::new(), // Forges determined at sync time
        ).with_description(options.description.unwrap_or_default());

        local_forge.create_repo(&desired).await
            .map_err(|e| ReposError::CreateError(e.to_string()))?;

        Ok(())
    }
}

/// Options for sync operation
#[derive(Debug, Clone, Default)]
pub struct SyncRepoOptions {
    pub dry_run: bool,
    pub delete_missing: bool,
    pub repo_filter: Option<HashSet<String>>,
}

/// Options for create operation
#[derive(Debug, Clone)]
pub struct CreateRepoOptions {
    pub visibility: Visibility,
    pub description: Option<String>,
}
```

### Service Command Updates

Update `src/service.rs` to wire up the new flow:

```rust
impl HyperforgeService {
    /// Handle org import command
    pub async fn handle_org_import(
        &self,
        org_name: &str,
        include_private: bool,
    ) -> Result<SyncReport, ServiceError> {
        // Get remote forge (e.g., GitHub)
        let remote_forge = self.get_forge(Forge::GitHub)?;

        // Get or create local forge with persistence
        let local_forge = self.get_or_create_local_forge(org_name)?;

        // Import = sync(remote, local)
        let options = ImportOptions {
            dry_run: false,
            include_private,
            ..Default::default()
        };

        self.org_activation.import(
            remote_forge.as_ref(),
            &local_forge,
            org_name,
            options,
        ).await.map_err(ServiceError::from)
    }

    /// Handle repos sync command
    pub async fn handle_repos_sync(
        &self,
        org_name: &str,
        target_forge: Forge,
        dry_run: bool,
    ) -> Result<SyncReport, ServiceError> {
        // Get local forge
        let local_forge = self.get_or_create_local_forge(org_name)?;

        // Get target remote forge
        let remote_forge = self.get_forge(target_forge)?;

        // Sync = sync(local, remote)
        let options = SyncRepoOptions {
            dry_run,
            delete_missing: false, // Require explicit flag for deletes
            ..Default::default()
        };

        self.repos_activation.sync(
            &local_forge,
            remote_forge.as_ref(),
            org_name,
            options,
        ).await.map_err(ServiceError::from)
    }

    fn get_or_create_local_forge(&self, org: &str) -> Result<LocalForge, ServiceError> {
        let path = self.config_dir.join("orgs").join(org).join("repos.yaml");
        LocalForge::with_auto_save(path, org.to_string())
            .map_err(|e| ServiceError::ConfigError(e.to_string()))
    }
}
```

### Integration Tests

```rust
#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::adapters::LocalForge;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_import_then_sync_roundtrip() {
        // Simulate: GitHub has repos, we import to local, then sync to Codeberg

        // Setup: GitHub with some repos
        let github = LocalForge::with_repos(vec![
            make_desired("myorg", "repo1"),
            make_desired("myorg", "repo2"),
        ]);

        // Empty local and codeberg
        let local = LocalForge::new();
        let codeberg = LocalForge::new();

        // Step 1: Import from GitHub to local
        let import_report = SymmetricSyncService::sync(
            &github,
            &local,
            "myorg",
            SyncOptions::new(),
        ).await.unwrap();

        assert_eq!(import_report.created_count(), 2);

        // Step 2: Sync from local to Codeberg
        let sync_report = SymmetricSyncService::sync(
            &local,
            &codeberg,
            "myorg",
            SyncOptions::new(),
        ).await.unwrap();

        assert_eq!(sync_report.created_count(), 2);

        // All three should now have the same repos
        assert_eq!(github.list_repos("myorg").await.unwrap().len(), 2);
        assert_eq!(local.list_repos("myorg").await.unwrap().len(), 2);
        assert_eq!(codeberg.list_repos("myorg").await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_import_preserves_existing_local_repos() {
        // Local has a repo that GitHub doesn't
        let github = LocalForge::with_repos(vec![
            make_desired("myorg", "from-github"),
        ]);
        let local = LocalForge::with_repos(vec![
            make_desired("myorg", "local-only"),
        ]);

        // Import without delete_missing
        SymmetricSyncService::sync(
            &github,
            &local,
            "myorg",
            SyncOptions::new(), // No delete_missing
        ).await.unwrap();

        // Local should have both repos
        let local_repos = local.list_repos("myorg").await.unwrap();
        assert_eq!(local_repos.len(), 2);
    }

    #[tokio::test]
    async fn test_sync_with_delete_missing() {
        // Local has subset, remote has extra
        let local = LocalForge::with_repos(vec![
            make_desired("myorg", "keep-this"),
        ]);
        let remote = LocalForge::with_repos(vec![
            make_desired("myorg", "keep-this"),
            make_desired("myorg", "delete-this"),
        ]);

        // Sync local -> remote with delete_missing
        SymmetricSyncService::sync(
            &local,
            &remote,
            "myorg",
            SyncOptions::new().delete_missing(),
        ).await.unwrap();

        // Remote should only have the repo from local
        let remote_repos = remote.list_repos("myorg").await.unwrap();
        assert_eq!(remote_repos.len(), 1);
        assert_eq!(remote_repos[0].identity.name, "keep-this");
    }

    #[tokio::test]
    async fn test_diff_command_uses_symmetric_sync_dry_run() {
        let local = LocalForge::with_repos(vec![
            make_desired("myorg", "new-repo"),
        ]);
        let remote = LocalForge::new();

        // Diff = sync with dry_run
        let report = SymmetricSyncService::sync(
            &local,
            &remote,
            "myorg",
            SyncOptions::new().dry_run(),
        ).await.unwrap();

        // Should show what would be created
        assert_eq!(report.results.len(), 1);
        assert!(report.results[0].action.is_create());
        assert!(matches!(report.results[0].outcome, SyncOutcome::Skipped));

        // But remote should still be empty
        assert!(remote.list_repos("myorg").await.unwrap().is_empty());
    }

    fn make_desired(org: &str, name: &str) -> DesiredRepo {
        use std::collections::HashSet;
        DesiredRepo::new(
            RepoIdentity::new(org, name),
            Visibility::Public,
            HashSet::new(),
        )
    }
}
```

### Key Design Decisions

1. **LocalForge as source of truth**: Local config is now a forge, not special
2. **Direction is explicit**: Import = remote->local, Sync = local->remote
3. **delete_missing opt-in**: Safe by default, must explicitly enable deletes
4. **Auto-save integration**: LocalForge persists changes automatically
5. **Service orchestrates**: Service wires up forges and activations
6. **Diff = dry_run sync**: No separate diff logic, just sync without applying

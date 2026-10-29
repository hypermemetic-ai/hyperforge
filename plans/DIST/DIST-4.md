# DIST-4: ReleasesService Subactivation

blocked_by: [DIST-2]
unlocks: [DIST-6, DIST-7, DIST-8]

## Scope

Create `ReleasesService` as a child subactivation of `RepoService`, following the `ImagesService` pattern exactly. Provides CRUD for forge releases and asset uploads via synapse CLI.

## Methods

### `releases list`
- Params: `org`, `name`, `forge` (optional)
- Lists all releases for a repo across configured forges
- Emits `ReleaseInfo` events

### `releases create`
- Params: `org`, `name`, `tag`, `title` (optional), `body` (optional), `draft` (optional), `prerelease` (optional), `forge` (optional)
- Creates a git tag if it doesn't exist
- Creates a release on each target forge via ReleasePort
- Emits `ReleaseCreate` events

### `releases upload`
- Params: `org`, `name`, `tag`, `file` (path to artifact), `forge` (optional)
- Finds release by tag, uploads file as asset
- Streams file content (don't load into memory for large binaries)
- Emits `ReleaseUpload` events

### `releases delete`
- Params: `org`, `name`, `tag`, `forge` (optional), `confirm` (default: dry-run)
- Deletes a release by tag
- Emits `ReleaseDelete` events

### `releases assets`
- Params: `org`, `name`, `tag`, `forge` (optional)
- Lists assets attached to a specific release
- Emits `AssetInfo` events

## Event Variants

Add to `HyperforgeEvent`:
```rust
ReleaseInfo { repo_name, forge, tag, title, asset_count, draft, prerelease, created_at }
ReleaseCreate { repo_name, forge, tag, success, error }
ReleaseUpload { repo_name, forge, tag, asset_name, size_bytes, success, error }
ReleaseDelete { repo_name, forge, tag, success, error }
```

## Wiring

- New file: `src/services/releases.rs`
- Add `pub mod releases;` to `src/services/mod.rs`
- Register as child of `RepoService` in `get_child("releases")`
- Add to `RepoService::plugin_children()` alongside ImagesService

## Usage

```bash
# List releases
synapse lforge hyperforge repo releases list --org hypermemetic --name hyperforge

# Create release
synapse lforge hyperforge repo releases create --org hypermemetic --name hyperforge --tag v4.1.0 --title "Hyperforge 4.1.0"

# Upload artifact
synapse lforge hyperforge repo releases upload --org hypermemetic --name hyperforge --tag v4.1.0 --file ./dist/hyperforge-x86_64-unknown-linux-gnu-v4.1.0.tar.gz

# List assets on a release
synapse lforge hyperforge repo releases assets --org hypermemetic --name hyperforge --tag v4.1.0
```

## Acceptance Criteria

- [ ] ReleasesService discoverable via `synapse lforge hyperforge repo releases`
- [ ] Can create a release on GitHub and Codeberg
- [ ] Can upload a binary file as a release asset
- [ ] Can list releases and their assets
- [ ] Delete is confirm-gated (dry-run by default)

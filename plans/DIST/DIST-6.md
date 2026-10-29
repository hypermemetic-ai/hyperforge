# DIST-6: `build release` All-in-One Orchestrator

blocked_by: [DIST-4, DIST-5]
unlocks: [DIST-9]

## Scope

A single `build release` command that cross-compiles, packages, creates a forge release, and uploads all artifacts. This is the primary user-facing command for the DIST epic.

## Method

Added to `BuildService` as a delegated free function in `src/services/build/release.rs`.

### Params
- `path` — workspace or repo path
- `tag` — git tag (e.g. `v4.1.0`)
- `targets` — comma-separated target triples (optional, defaults to native)
- `include` / `exclude` — repo name filters (workspace mode)
- `forge` — target forges (optional, defaults to all configured)
- `title` — release title (optional, defaults to tag)
- `body` — release notes (optional)
- `draft` — create as draft (optional, default: false)
- `dry_run` — preview everything (optional, default: false)

### Flow

```
1. Discover repo(s) from path
2. For each repo:
   a. Detect build system + binary targets (DIST-3)
   b. Read version from manifest
   c. For each target triple:
      - Compile (native or cross) (DIST-5)
      - Package into archive with binstall naming (DIST-5)
   d. Create git tag if not exists
   e. For each forge:
      - Create release via ReleasePort (DIST-4)
      - Upload each archive as asset (DIST-4)
3. Emit summary
```

### Events

```rust
ReleaseBuildStep {
    repo_name: String,
    target: String,
    status: String,  // "compiling", "packaging", "uploading"
    detail: Option<String>,
}

ReleaseSummary {
    repos: usize,
    targets: usize,
    forges: usize,
    assets_uploaded: usize,
    failed: usize,
}
```

## Usage

```bash
# Release a single repo for native target
synapse lforge hyperforge build release \
  --path /path/to/hyperforge --tag v4.1.0

# Release with cross-compilation
synapse lforge hyperforge build release \
  --path /path/to/hyperforge --tag v4.1.0 \
  --targets x86_64-unknown-linux-gnu,aarch64-apple-darwin

# Workspace-wide release
synapse lforge hyperforge build release \
  --path /path/to/workspace --tag v4.1.0 \
  --include "plexus-*" --targets x86_64-unknown-linux-gnu

# Dry run
synapse lforge hyperforge build release \
  --path . --tag v4.1.0 --dry_run true
```

## Acceptance Criteria

- [ ] Single command builds, packages, creates release, uploads assets
- [ ] Works for single repo and workspace (with filters)
- [ ] Cross-compilation for at least 2 target triples
- [ ] GitHub and Codeberg releases created with matching assets
- [ ] Dry-run shows full plan without side effects
- [ ] `cargo binstall {crate}` works after release

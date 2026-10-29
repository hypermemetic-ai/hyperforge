# DIST-1: Multi-Channel Binary Distribution

## Goal

Enable hyperforge to build, package, and distribute pre-built binaries across multiple channels — forge releases (cargo binstall), Homebrew, and future package managers — for both Rust and Haskell projects in the workspace. Distribution targets are configured per-repo.

## Status

| Ticket | Status | Description |
|--------|--------|-------------|
| DIST-2 | **DONE** | ReleasePort trait + GitHub/Codeberg adapters |
| DIST-3 | **DONE** | Binary target detection from Cargo.toml/.cabal |
| DIST-4 | **DONE** | ReleasesService subactivation (list/create/upload/delete/assets) |
| DIST-5 | **DONE** | Cross-compile engine + tar.gz packaging |
| DIST-6 | **DONE** | `build release` all-in-one orchestrator |
| DIST-7 | **DONE** | Homebrew formula generation |
| DIST-8 | **DONE** | Binstall metadata injection |
| DIST-9 | **DONE** | Workspace-wide release with dependency ordering |
| DIST-10 | IN PROGRESS | Distribution config as source of truth |

## Context

Today hyperforge can publish **source packages** (crates.io via `package_publish`, Hackage via `package_publish`) and **container images** (ghcr.io/codeberg via `ImagesService`). DIST adds **pre-built binary distribution**.

### Language-specific considerations

| Language | Source publish | Binary install | Cross-compile |
|----------|--------------|----------------|---------------|
| **Rust** | crates.io | cargo binstall (forge releases) | `cross` crate or `cargo build --target` |
| **Haskell** | Hackage | Homebrew, direct download | Native only; Docker for Linux targets |

## Dependency DAG

```
DIST-2 (ReleasePort trait + adapters)         ✅
  │
  ├──► DIST-4 (ReleasesService subactivation)     ✅
  │      │
  │      ├──► DIST-7 (Homebrew formulas)      ✅
  │      └──► DIST-8 (Binstall metadata)      ✅
  │
  └──► DIST-6 (build release orchestrator)    ✅
         ▲          │
         │          └──► DIST-9 (workspace)   ✅
         │
DIST-3 (binary detection)                     ✅
  │
  └──► DIST-5 (cross-compile engine)          ✅

DIST-10 (dist config as source of truth)      🔧 IN PROGRESS
  └── depends on DIST-6
  └── makes release/release_all config-driven
```

## Commands (all implemented)

```bash
# Per-repo release management
synapse lforge hyperforge repo releases list --org x --name y
synapse lforge hyperforge repo releases create --org x --name y --tag v1.0
synapse lforge hyperforge repo releases upload --org x --name y --tag v1.0 --file ./app.tar.gz
synapse lforge hyperforge repo releases assets --org x --name y --tag v1.0
synapse lforge hyperforge repo releases delete --org x --name y --tag v1.0 --confirm true

# Single-repo build + release
synapse lforge hyperforge build release --path . --tag v1.0 --targets x86_64-unknown-linux-gnu,aarch64-apple-darwin

# Workspace-wide release (dependency ordered)
synapse lforge hyperforge build release_all --path . --tag v1.0

# Package manager integration
synapse lforge hyperforge build brew_formula --org x --name y --tag v1.0 --tap_path /path/to/tap
synapse lforge hyperforge build binstall_init --path . --forge github

# Coming (DIST-10):
synapse lforge hyperforge build dist_show --path .
synapse lforge hyperforge build dist_init --path .
```

## Success Criteria

- [x] `build release` builds and uploads binaries to forge releases
- [x] `cargo binstall {crate}` installable after binstall_init + release
- [x] `brew install org/tap/name` installable after brew_formula
- [x] Both GitHub and Codeberg releases supported
- [x] Workspace-wide release in dependency order
- [ ] Distribution config per-repo (DIST-10)

# LFORGE2-0: Project Setup and Worktree Creation

**Status**: Ready to implement
**Blocked by**: None
**Unlocks**: LFORGE2-2

---

## Goal

Set up fresh hyperforge implementation in a git worktree, starting from scratch.

---

## Tasks

### 1. Create Worktree

```bash
cd ~/dev/controlflow/hypermemetic

# Create worktree for hyperforge redesign
git worktree add ../hyperforge-lforge2 -b feat/lforge2-redesign

cd ../hyperforge-lforge2/hyperforge
```

### 2. Delete All Source

```bash
# Keep build configuration, delete implementation
rm -rf src/*
rm -rf tests/*

# Keep Cargo.toml but update it
```

### 3. Update Cargo.toml

```toml
[package]
name = "hyperforge"
version = "2.0.0"
edition = "2021"

[dependencies]
# Service framework integration
plexus-core = { path = "../plexus-core" }
plexus-macros = { path = "../plexus-macros" }

# Async
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
async-stream = "0.3"
futures = "0.3"

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
toml_edit = "0.22"  # Preserve formatting when editing

# Error handling
anyhow = "1"
thiserror = "1"

# Git operations
regex = "1"

# Auth (WorkOS)
workos = "0.5"  # https://docs.rs/workos/latest/workos/

# CLI (if needed for testing)
clap = { version = "4", features = ["derive"] }

[dev-dependencies]
tempfile = "3"
```

### 4. Create Initial Directory Structure

```bash
mkdir -p src/{types,config,git,package,remote,auth}
mkdir -p tests/{integration,fixtures}

# Placeholder files
touch src/lib.rs
touch src/types/mod.rs
touch src/config/mod.rs
touch src/git/mod.rs
touch src/package/mod.rs
touch src/remote/mod.rs
touch src/auth/mod.rs
```

### 5. Initial lib.rs

```rust
// src/lib.rs

//! Hyperforge - Multi-forge repository management
//!
//! Hyperforge manages repositories across multiple git forges (GitHub, Codeberg, GitLab)
//! using declarative configuration and git as the source of truth.

pub mod auth;
pub mod config;
pub mod git;
pub mod package;
pub mod remote;
pub mod types;

// Re-exports for convenience
pub use config::HyperforgeConfig;
pub use types::*;
```

### 6. Verify Compilation

```bash
cd ~/dev/controlflow/hypermemetic/hyperforge-lforge2/hyperforge
cargo build

# Should compile with no errors (empty modules)
```

### 7. Create Initial Test

```rust
// tests/integration/smoke_test.rs

#[test]
fn it_compiles() {
    // Just verify the crate loads
    assert!(true);
}
```

```bash
cargo test
# Should pass
```

---

## Acceptance Criteria

- ✅ Worktree created at `../hyperforge-lforge2`
- ✅ Branch `feat/lforge2-redesign` created
- ✅ All old source deleted (`src/*`, `tests/*`)
- ✅ `Cargo.toml` updated with correct dependencies
- ✅ Directory structure created
- ✅ `cargo build` succeeds
- ✅ `cargo test` passes (smoke test)

---

## Commit Message

```
chore: initialize LFORGE2 hyperforge redesign

- Create fresh worktree for complete redesign
- Delete all existing source code
- Set up module structure (types, config, git, package, remote, auth)
- Add dependencies: plexus-core, workos, toml_edit, etc.
- Smoke test passes

Starting fresh implementation of repo-local, git-native hyperforge.

Co-Authored-By: Claude Sonnet 4.5 <noreply@anthropic.com>
```

---

## Next Steps

After this ticket, proceed to **LFORGE2-2: Core types and configuration**.

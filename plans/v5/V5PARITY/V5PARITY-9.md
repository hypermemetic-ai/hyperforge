---
id: V5PARITY-9
title: "BUILD-MANIFEST — unify, analyze, validate, detect_name_mismatches, package_diff"
status: Complete
type: implementation
blocked_by: [V5PARITY-2]
unlocks: [V5PARITY-10, V5PARITY-11, V5PARITY-12]
---

## Problem

v4's `BuildService` surfaces manifest inspection across a multi-repo workspace (Cargo.toml + package.json unified analysis, drift detection, name-mismatch checks). v5 has zero build surface.

## Required behavior

**New activation: `BuildService`** — static child on `HyperforgeService`.

**New module tree: `src/v5/build/`** — sibling of `ops/`. Submodules: `manifest.rs`, `validate.rs`, `diff.rs`. Pure functions operating on a workspace path.

| Method | Behavior |
|---|---|
| `build.unify --path P` | Walk all members of the workspace at P (or of the workspace named via `--name`); for each, parse `Cargo.toml` / `package.json` / `pyproject.toml`; emit unified `package_manifest { ref, name, version, deps: [{name, version, source}] }` events. Output is consumable by downstream dependency-graph tools. |
| `build.analyze --name W` | Build a cross-repo dep graph from `unify`'s output; emit `cycle`, `duplicate_name`, `version_mismatch` events for anomalies. |
| `build.validate --name W` | Stricter: each manifest is parseable, every dep resolution is reachable (via configured providers), no same-name-different-version across workspace. Emits typed violations + aggregate pass/fail. |
| `build.detect_name_mismatches --name W` | Scans each member's manifest for `name` vs the workspace ref (`<org>/<repo>`). Emits mismatches. |
| `build.package_diff --name W --from_ref R1 --to_ref R2` | Diffs the unified manifest between two git refs (requires V5PARITY-3's `ops::git` for `git show R:path`). Emits added/removed/version-changed per package. |

## What must NOT change

- Workspace sync / reconcile semantics.
- `ops::git` from V5PARITY-3 is the only subprocess entry point for git operations; `package_diff` uses it too.
- D13 — `build/*` uses `ops::*` for all state + subprocess needs.

## Acceptance criteria

1. `build.unify --name W` on a workspace with Rust + JS members emits a manifest event per member with correct `name` + `version`.
2. `build.analyze` on a workspace with a version mismatch (e.g. two members depend on `serde 1.0.200` and `serde 1.0.150`) emits a `version_mismatch` event.
3. `build.validate` on a well-formed workspace passes; on a workspace with a broken manifest (unparseable `Cargo.toml`) emits a typed `manifest_parse_error`.
4. `build.detect_name_mismatches` flags repos whose manifest `name` differs from the workspace's `<repo_name>` segment.
5. `build.package_diff --from_ref HEAD~1 --to_ref HEAD` on a workspace with a recent version bump emits the version-changed event.

## Completion

- Run `bash tests/v5/V5PARITY/V5PARITY-9.sh` → exit 0 (tier 1 — uses fixture checkouts with known manifests).
- Ready → Complete in-commit.

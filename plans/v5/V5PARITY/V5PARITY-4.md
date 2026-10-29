---
id: V5PARITY-4
title: "ANALYTICS — size, loc, large_files, dirty + workspace aggregates"
status: Complete
type: implementation
blocked_by: [V5PARITY-2]
unlocks: [V5PARITY-12]
---

## Problem

v4 has `repo.{size, loc, large_files, dirty}` for quick repo inspection + `workspace.repo_sizes` for workspace-wide rollups. v5 has none of this.

## Required behavior

**New module: `src/v5/ops/analytics.rs`** — pure functions over a checkout directory:

| Function | Returns |
|---|---|
| `repo_size(dir)` | `{ bytes: u64, file_count: u64 }` — walks the tree excluding `.git/` |
| `repo_loc(dir)` | `BTreeMap<Language, u64>` — counts lines per detected language; use `tokei` crate if already in tree, else simple extension-based heuristic |
| `large_files(dir, threshold_bytes)` | `Vec<{path, size}>` — files above threshold |
| `repo_dirty(dir)` | reuse `ops::git::is_dirty` (added in V5PARITY-3) |

**RPC methods on ReposService:**

| Method | Emits |
|---|---|
| `repos.size --path P` | `repo_size_summary { bytes, file_count }` |
| `repos.loc --path P` | `repo_loc_summary { by_language: {lang: lines} }` |
| `repos.large_files --path P [--threshold KB]` | stream of `large_file { path, size }` + `large_files_summary` |
| `repos.dirty --path P` | `repo_dirty { dirty }` (alias for V5PARITY-3's method — one implementation) |

**Workspace-level aggregates:** `workspaces.repo_sizes`, `workspaces.loc`, `workspaces.large_files`, `workspaces.dirty` — iterate members + emit per-member + aggregate. Same bounded parallelism pattern as V5PARITY-3.

## What must NOT change

- V5PARITY-3's `repos.dirty` stays; this ticket's `repos.dirty` is the same method (don't introduce a duplicate). Only one implementation lives in `ops::git::is_dirty`; both tickets' acceptance tests the same callsite.
- `ops::analytics` doesn't shell out to `git` (except dirty); it's pure filesystem walk.

## Acceptance criteria

1. `repos.size --path <clone>` returns file_count + bytes that match `find <clone> -type f -not -path '*/\\.git/*' | wc -l` and `du -sb`.
2. `repos.loc --path <clone>` returns per-language counts; sum matches `cloc` within 5%.
3. `repos.large_files --path <clone> --threshold 100` lists files ≥100 KB only.
4. `workspaces.repo_sizes --name W` emits per-member sizes + an aggregate `workspace_size_summary`.

## Completion

- Run `bash tests/v5/V5PARITY/V5PARITY-4.sh` → exit 0 (tier 1 — uses a fixture checkout with known sizes).
- Ready → Complete in-commit.

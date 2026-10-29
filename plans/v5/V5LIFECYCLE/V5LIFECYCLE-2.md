---
id: V5LIFECYCLE-2
title: "ops::state — shared yaml I/O, lookups, atomic mutation"
status: Complete
type: implementation
blocked_by: []
unlocks: [V5LIFECYCLE-3, V5LIFECYCLE-4]
---

## Problem

Today every service that touches yaml re-implements: loading config.yaml + orgs/*.yaml + workspaces/*.yaml, finding a repo in an org, atomically saving an org/workspace yaml. Per D13 this must live in one place.

## Required behavior

Introduce a `src/v5/ops/state` module (exact filename free to the implementer) exposing a small set of functions. **No behavior change** — every existing callsite behaves identically before and after this ticket.

Required function surface (names indicative; signatures are the contract):

| Capability | Inputs | Output |
|---|---|---|
| load all | `config_dir: &Path` | `LoadedConfig` (existing type; extracted as-is) |
| load orgs | `config_dir: &Path` | `BTreeMap<OrgName, OrgConfig>` |
| load workspaces | `config_dir: &Path` | `BTreeMap<WorkspaceName, WorkspaceConfig>` |
| load one workspace | `config_dir: &Path`, `name: &WorkspaceName` | `Option<WorkspaceConfig>` |
| find repo | `&OrgConfig`, `&RepoName` | `Option<&OrgRepo>` |
| find repo mut | `&mut OrgConfig`, `&RepoName` | `Option<&mut OrgRepo>` |
| save org | `config_dir: &Path`, `&OrgConfig` | `Result<(), ConfigError>` — atomic per D8 |
| save workspace | `config_dir: &Path`, `&WorkspaceConfig` | `Result<(), ConfigError>` — atomic per D8 |
| delete org file | `config_dir: &Path`, `&OrgName` | `Result<(), ConfigError>` |
| delete workspace file | `config_dir: &Path`, `&WorkspaceName` | `Result<(), ConfigError>` |

Migration:
- Every existing `load_*` / `save_*` / `find_repo*` call inside `src/v5/repos.rs`, `src/v5/workspaces.rs`, `src/v5/orgs.rs`, and `src/v5/service.rs` routes through this module.
- The old functions in `src/v5/config.rs` either become thin re-exports from `ops::state` or are removed entirely. Implementer's call.

## What must NOT change

- The full v5 tier-1 test sweep (currently ~34 scripts) passes byte-identically after this ticket.
- Wire event shapes — nothing RPC-observable moves.
- D8 atomicity guarantee — every save still goes through "tempfile + rename".

## Acceptance criteria

1. `cargo test --test v5_integration` tier-1 passes green; count matches pre-ticket count.
2. `grep -RE 'serde_yaml|fs::(read_to_string|write|create_dir)' src/v5/ | grep -v '^src/v5/ops/'` returns empty (or only matches allowed paths: `src/v5/secrets.rs` for the YAML-backed secret store is exempt — it's state of a different kind).
3. Every `src/v5/{repos,workspaces,orgs,service}.rs` file references `ops::state::*` for at least one yaml interaction; none reference `load_all` / `save_org` / `load_workspaces` etc. via their old paths.
4. A repo lookup across all callsites uses the same `find_repo` / `find_repo_mut` — no parallel implementations.

## Completion

- Run `bash tests/v5/V5LIFECYCLE/V5LIFECYCLE-2.sh` → exit 0.
- Status flips in-commit.

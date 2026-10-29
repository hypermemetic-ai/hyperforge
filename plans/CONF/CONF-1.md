# CONF-1: Config-First Architecture + Workspace Concurrency

## Goal

Eliminate config staleness by making LocalForge the single source of truth for all repo state. Per-repo `.hyperforge/config.toml` and git remotes become projections of LocalForge, not independent stores. Simultaneously, extract a `WorkspaceRunner` abstraction to deduplicate the 6+ parallel execution patterns in `workspace.rs`.

## Context

Today there are three config layers that drift independently:

| Layer | Location | Written by | Read by |
|-------|----------|-----------|---------|
| Per-repo config | `{repo}/.hyperforge/config.toml` | init, move_repos | discover, push, status |
| LocalForge | `~/.config/hyperforge/orgs/{org}/repos.yaml` | create, import, sync, move | list, clone, diff, sync, publish |
| Git remotes | `{repo}/.git/config` | init, move_repos | push, status, check |

8 documented disconnects exist where writing one layer doesn't update the others. The config-first model makes this structurally impossible: all mutations flow through LocalForge, and side effects are derived via a `materialize` step.

## Dependency DAG

```
CONF-2 (RepoRecord expansion)
  ├──► CONF-3 (materialize function)
  │      ├──► CONF-5 (config-first repo operations)
  │      └──► CONF-6 (config-first workspace operations)
  │
  └──► CONF-4 (WorkspaceRunner abstraction)
         └──► CONF-6 (config-first workspace operations)

CONF-7 (cleanup & migration) ◄── CONF-5, CONF-6
```

## Phases

### Phase 1: Foundation (CONF-2, CONF-4) — parallelizable
- Expand `RepoRecord` to absorb per-repo config fields
- Extract `WorkspaceRunner` concurrency abstraction

### Phase 2: Core Mechanism (CONF-3) — depends on CONF-2
- Implement `materialize(record, path)` that projects LocalForge state to disk (config file + git remotes)

### Phase 3: Rewire Operations (CONF-5, CONF-6) — depends on CONF-3, CONF-4
- Rewire repo service methods to go through LocalForge-first
- Rewire workspace service methods to use WorkspaceRunner + config-first flow

### Phase 4: Cleanup (CONF-7) — depends on CONF-5, CONF-6
- Remove dead code, migrate existing repos, update docs

## Estimated Impact

| Metric | Before | After |
|--------|--------|-------|
| workspace.rs lines | ~3800 | ~2200-2500 |
| Config disconnect points | 8 | 0 |
| Parallel execution copy-pastes | 6 | 0 (1 abstraction) |
| Per-repo config writes | 3 ad-hoc locations | 1 (`materialize`) |

## Tickets

- [CONF-2](CONF-2.md) — Expand RepoRecord to absorb per-repo config fields
- [CONF-3](CONF-3.md) — Implement `materialize` config projection
- [CONF-4](CONF-4.md) — Extract WorkspaceRunner concurrency abstraction
- [CONF-5](CONF-5.md) — Config-first repo service operations
- [CONF-6](CONF-6.md) — Config-first workspace service operations + WorkspaceRunner adoption
- [CONF-7](CONF-7.md) — Cleanup, migration, dead code removal
